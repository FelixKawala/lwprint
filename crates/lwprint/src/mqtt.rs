//! MQTT integration with Home Assistant discovery.
//!
//! Topics (with the default base topic "lwprint"):
//!   lwprint/availability   "online" / "offline" (retained, last will)
//!   lwprint/state          printer status JSON (retained)
//!   lwprint/print          publish a label JSON here to print it
//!   lwprint/result         result of every finished job (and rejected requests)
//!   lwprint/refresh        publish anything to check the printer now
//!   lwprint/cancel         "all", a job id, or a request_id: stop printing
//!   lwprint/hold           "on": hold labels instead of printing, "off": print held labels

use crate::config::MqttConfig;
use crate::service::{log, Event, PrintRequest, Service};
use rumqttc::{AsyncClient, Event as MqttEvent, LastWill, MqttOptions, Packet, QoS};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub async fn run(svc: Arc<Service>, cfg: MqttConfig) {
    let base = cfg.base_topic.trim_end_matches('/').to_string();
    let mut opts = MqttOptions::new(svc.device_id(), cfg.host.clone(), cfg.port);
    opts.set_keep_alive(Duration::from_secs(30));
    opts.set_max_packet_size(16 * 1024 * 1024, 1024 * 1024);
    opts.set_last_will(LastWill::new(format!("{base}/availability"), "offline", QoS::AtLeastOnce, true));
    if let Some(user) = &cfg.username {
        opts.set_credentials(user.clone(), cfg.password.clone().unwrap_or_default());
    }
    let (client, mut eventloop) = AsyncClient::new(opts, 64);

    // Forward service events (status changes, finished jobs) to MQTT.
    {
        let client = client.clone();
        let base = base.clone();
        let mut events = svc.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::Status(st)) => {
                        let payload = state_payload(&serde_json::to_value(&st).unwrap_or_default());
                        let _ = client.publish(format!("{base}/state"), QoS::AtLeastOnce, true, payload).await;
                    }
                    Ok(Event::JobFinished(job)) => {
                        let payload = serde_json::to_vec(&job).unwrap_or_default();
                        let _ = client.publish(format!("{base}/result"), QoS::AtLeastOnce, false, payload).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    loop {
        match eventloop.poll().await {
            Ok(MqttEvent::Incoming(Packet::ConnAck(_))) => {
                log(format!("MQTT: connected to {}:{}", cfg.host, cfg.port));
                let (client, svc, cfg, base) = (client.clone(), svc.clone(), cfg.clone(), base.clone());
                tokio::spawn(async move { on_connect(&client, &svc, &cfg, &base).await });
            }
            Ok(MqttEvent::Incoming(Packet::Publish(p))) => {
                if p.topic == format!("{base}/print") {
                    if p.retain {
                        // A retained print message would print again on every restart.
                        log("MQTT: ignoring retained message on the print topic (publish without retain)");
                    } else {
                        handle_print(&svc, &client, &base, &p.payload).await;
                    }
                } else if p.topic == format!("{base}/refresh") {
                    svc.request_refresh();
                } else if p.topic == format!("{base}/cancel") && !p.retain {
                    handle_cancel(&svc, &p.payload);
                } else if p.topic == format!("{base}/hold") && !p.retain {
                    match String::from_utf8_lossy(&p.payload).trim().to_ascii_lowercase().as_str() {
                        "on" | "true" | "1" | "hold" => svc.set_held(true),
                        "off" | "false" | "0" | "release" => svc.set_held(false),
                        other => log(format!("MQTT: unknown hold command '{other}' (use on/off)")),
                    }
                }
            }
            Ok(_) => {}
            Err(e) => {
                log(format!("MQTT: {e}; retrying in 10 s"));
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

/// Make empty values explicit so Home Assistant templates stay simple.
fn state_payload(st: &Value) -> Vec<u8> {
    let mut v = st.clone();
    if v["problem"].is_null() {
        v["problem"] = json!("none");
    }
    if v["last_result"].is_null() {
        v["last_result"] = json!("none");
    }
    serde_json::to_vec(&v).unwrap_or_default()
}

async fn handle_print(svc: &Service, client: &AsyncClient, base: &str, payload: &[u8]) {
    // A list of labels prints as one printer job.
    if payload.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'[') {
        let result = serde_json::from_slice::<Vec<PrintRequest>>(payload)
            .map_err(|e| format!("invalid label JSON: {e}"))
            .and_then(|reqs| svc.submit_all(reqs, "mqtt").map_err(|e| format!("{e:#}")));
        if let Err(message) = result {
            log(format!("MQTT: rejected print request: {message}"));
            let payload = json!({ "state": "rejected", "request_id": null, "message": message });
            let _ = client.publish(format!("{base}/result"), QoS::AtLeastOnce, false, payload.to_string()).await;
        }
        return;
    }
    let result = serde_json::from_slice::<PrintRequest>(payload)
        .map_err(|e| (None, format!("invalid label JSON: {e}")))
        .and_then(|req| {
            let rid = req.request_id.clone();
            svc.submit(req, "mqtt").map_err(|e| (rid, format!("{e:#}")))
        });
    if let Err((request_id, message)) = result {
        log(format!("MQTT: rejected print request: {message}"));
        let payload = json!({ "state": "rejected", "request_id": request_id, "message": message });
        let _ = client.publish(format!("{base}/result"), QoS::AtLeastOnce, false, payload.to_string()).await;
    }
}

/// Stop jobs: payload "all", a job id, or a request_id given when printing.
fn handle_cancel(svc: &Service, payload: &[u8]) {
    let text = String::from_utf8_lossy(payload).trim().trim_matches('"').to_string();
    if text.is_empty() || text.eq_ignore_ascii_case("all") {
        let n = svc.cancel_all();
        log(format!("MQTT: stop requested for all jobs ({n} running)"));
        return;
    }
    let ids: Vec<u64> = match text.parse::<u64>() {
        Ok(id) => vec![id],
        Err(_) => svc
            .jobs()
            .into_iter()
            .filter(|j| j.state.is_active() && j.request_id.as_deref() == Some(text.as_str()))
            .map(|j| j.id)
            .collect(),
    };
    for id in ids {
        let _ = svc.cancel(id);
    }
}

async fn on_connect(client: &AsyncClient, svc: &Service, cfg: &MqttConfig, base: &str) {
    let _ = client.subscribe(format!("{base}/print"), QoS::AtLeastOnce).await;
    let _ = client.subscribe(format!("{base}/refresh"), QoS::AtLeastOnce).await;
    let _ = client.subscribe(format!("{base}/cancel"), QoS::AtLeastOnce).await;
    let _ = client.subscribe(format!("{base}/hold"), QoS::AtLeastOnce).await;
    if !cfg.discovery_prefix.is_empty() {
        for (topic, payload) in discovery(svc, &cfg.discovery_prefix, base) {
            let _ = client.publish(topic, QoS::AtLeastOnce, true, payload.to_string()).await;
        }
    }
    let _ = client.publish(format!("{base}/availability"), QoS::AtLeastOnce, true, "online").await;
    let st = serde_json::to_value(svc.status()).unwrap_or_default();
    let _ = client.publish(format!("{base}/state"), QoS::AtLeastOnce, true, state_payload(&st)).await;
}

/// Home Assistant MQTT discovery messages: one device with sensors and a refresh button.
fn discovery(svc: &Service, prefix: &str, base: &str) -> Vec<(String, Value)> {
    let id = svc.device_id();
    let device = json!({
        "identifiers": [id],
        "name": "Label printer",
        "manufacturer": "Epson",
        "model": "LabelWorks LW-C410",
        "sw_version": env!("CARGO_PKG_VERSION"),
    });
    let common = |name: &str, key: &str| {
        json!({
            "name": name,
            "unique_id": format!("{id}_{key}"),
            "object_id": format!("label_printer_{key}"),
            "device": device,
            "availability_topic": format!("{base}/availability"),
            "state_topic": format!("{base}/state"),
        })
    };
    let mut out = Vec::new();
    let mut add = |component: &str, key: &str, mut cfg: Value, extra: Value| {
        if let (Some(c), Some(e)) = (cfg.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                c.insert(k.clone(), v.clone());
            }
        }
        out.push((format!("{prefix}/{component}/{id}/{key}/config"), cfg));
    };
    add(
        "binary_sensor",
        "online",
        common("Printer reachable", "online"),
        json!({
            "device_class": "connectivity",
            "value_template": "{{ 'ON' if value_json.printer_online else 'OFF' }}",
        }),
    );
    add(
        "sensor",
        "tape",
        common("Tape width", "tape"),
        json!({
            "unit_of_measurement": "mm",
            "icon": "mdi:label-outline",
            "value_template": "{{ value_json.tape_mm if value_json.tape_mm else 'unknown' }}",
        }),
    );
    add(
        "sensor",
        "state",
        common("State", "state"),
        json!({ "icon": "mdi:printer", "value_template": "{{ value_json.state }}" }),
    );
    add(
        "sensor",
        "problem",
        common("Problem", "problem"),
        json!({ "icon": "mdi:alert-circle-outline", "value_template": "{{ value_json.problem }}" }),
    );
    add(
        "sensor",
        "queue",
        common("Labels waiting", "queue"),
        json!({ "icon": "mdi:tray-full", "value_template": "{{ value_json.queue }}" }),
    );
    add(
        "sensor",
        "last_result",
        common("Last print", "last_result"),
        json!({ "icon": "mdi:history", "value_template": "{{ value_json.last_result[:250] }}" }),
    );
    let mut button = common("Check printer", "refresh");
    if let Some(o) = button.as_object_mut() {
        o.remove("state_topic");
        o.insert("command_topic".into(), json!(format!("{base}/refresh")));
        o.insert("payload_press".into(), json!("refresh"));
        o.insert("icon".into(), json!("mdi:refresh"));
    }
    out.push((format!("{prefix}/button/{id}/refresh/config"), button));
    let mut stop = common("Cancel printing", "cancel");
    if let Some(o) = stop.as_object_mut() {
        o.remove("state_topic");
        o.insert("command_topic".into(), json!(format!("{base}/cancel")));
        o.insert("payload_press".into(), json!("all"));
        o.insert("icon".into(), json!("mdi:printer-off"));
    }
    out.push((format!("{prefix}/button/{id}/cancel/config"), stop));
    let mut hold = common("Hold labels", "hold");
    if let Some(o) = hold.as_object_mut() {
        o.insert("command_topic".into(), json!(format!("{base}/hold")));
        o.insert("payload_on".into(), json!("on"));
        o.insert("payload_off".into(), json!("off"));
        o.insert("value_template".into(), json!("{{ 'on' if value_json.held else 'off' }}"));
        o.insert("state_on".into(), json!("on"));
        o.insert("state_off".into(), json!("off"));
        o.insert("icon".into(), json!("mdi:pause-circle-outline"));
    }
    out.push((format!("{prefix}/switch/{id}/hold/config"), hold));
    out
}
