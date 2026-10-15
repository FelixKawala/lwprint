"""Run selected CommandFilter methods from libLWPrint.so (arm64) under Unicorn."""
import struct
from elftools.elf.elffile import ELFFile
from elftools.elf.relocation import RelocationSection
from unicorn import *
from unicorn.arm64_const import *

import os
LIB = os.environ.get('LWPRINT_SO', 'libLWPrint.so')  # arm64-v8a build from the app's split APK
BASE = 0x10000000
STUB = 0x20000000
HEAP = 0x30000000
STACK = 0x40000000
TLS = 0x50000000
SCRATCH = 0x60000000
JNI = 0x70000000
RET_ADDR = STUB + 0xFFF0


class Emu:
    def __init__(self):
        f = open(LIB, 'rb')
        self.elf = ELFFile(f)
        self.raw = open(LIB, 'rb').read()
        mu = self.mu = Uc(UC_ARCH_ARM64, UC_MODE_ARM)
        # load segments
        hi = 0
        for s in self.elf.iter_segments():
            if s['p_type'] == 'PT_LOAD':
                hi = max(hi, s['p_vaddr'] + s['p_memsz'])
        mu.mem_map(BASE, (hi + 0xFFFF) & ~0xFFFF)
        for s in self.elf.iter_segments():
            if s['p_type'] == 'PT_LOAD':
                mu.mem_write(BASE + s['p_vaddr'], self.raw[s['p_offset']:s['p_offset'] + s['p_filesz']])
        for base, size in [(STUB, 0x10000), (HEAP, 0x1000000), (STACK - 0x100000, 0x100000),
                           (TLS, 0x1000), (SCRATCH, 0x100000), (JNI, 0x10000)]:
            mu.mem_map(base, size)
        # every stub slot is a RET
        mu.mem_write(STUB, b'\xc0\x03\x5f\xd6' * (0x10000 // 4))
        mu.reg_write(UC_ARM64_REG_TPIDR_EL0, TLS)
        # fake JNIEnv: function table of RET stubs returning 0
        mu.mem_write(JNI, struct.pack('<Q', JNI + 0x100))
        for i in range(300):
            mu.mem_write(JNI + 0x100 + 8 * i, struct.pack('<Q', STUB + 0x8000))
        self.heap_ptr = HEAP
        self.stubs = {}
        self.syms = {}
        dynsym = self.elf.get_section_by_name('.dynsym')
        for s in dynsym.iter_symbols():
            if s['st_value']:
                self.syms[s.name] = s['st_value']
        n = 0
        for sec in self.elf.iter_sections():
            if not isinstance(sec, RelocationSection):
                continue
            for r in sec.iter_relocations():
                t = r['r_info_type']
                addr = BASE + r['r_offset']
                if t == 1027:  # RELATIVE
                    val = BASE + r['r_addend']
                else:
                    sym = dynsym.get_symbol(r['r_info_sym'])
                    if sym['st_value']:
                        val = BASE + sym['st_value']
                    else:
                        if sym.name not in self.stubs.values():
                            n += 1
                            self.stubs[STUB + 4 * n] = sym.name
                        val = [a for a, nm in self.stubs.items() if nm == sym.name][0]
                    if t == 257:
                        val += r['r_addend']
                mu.mem_write(addr, struct.pack('<Q', val))
        mu.hook_add(UC_HOOK_CODE, self._stub_hook, begin=STUB, end=STUB + 0xFFFF)
        self.log = []

    def malloc(self, n):
        p = self.heap_ptr
        self.heap_ptr += (n + 15) & ~15
        self.mu.mem_write(p, b'\0' * n)
        return p

    def _stub_hook(self, mu, addr, size, ud):
        if addr == RET_ADDR:
            mu.emu_stop()
            return
        name = self.stubs.get(addr)
        x = [mu.reg_read(UC_ARM64_REG_X0 + i) for i in range(4)]
        if name in ('malloc', '_Znwm', '_Znam'):
            mu.reg_write(UC_ARM64_REG_X0, self.malloc(x[0]))
        elif name in ('calloc',):
            mu.reg_write(UC_ARM64_REG_X0, self.malloc(x[0] * x[1]))
        elif name in ('memcpy', 'memmove'):
            mu.mem_write(x[0], bytes(mu.mem_read(x[1], x[2])))
        elif name == 'memset':
            mu.mem_write(x[0], bytes([x[1] & 0xff]) * x[2])
        elif name == 'strlen':
            i = 0
            while mu.mem_read(x[0] + i, 1)[0]:
                i += 1
            mu.reg_write(UC_ARM64_REG_X0, i)
        elif name == 'memcmp':
            a = bytes(mu.mem_read(x[0], x[2])); b = bytes(mu.mem_read(x[1], x[2]))
            mu.reg_write(UC_ARM64_REG_X0, 0 if a == b else (1 if a > b else 0xFFFFFFFFFFFFFFFF))
        elif name in ('free', '_ZdlPv', '_ZdaPv', None):
            pass
        else:
            self.log.append(name)

    def call(self, fn, *args):
        addr = BASE + (self.syms[fn] if isinstance(fn, str) else fn)
        mu = self.mu
        for i, a in enumerate(args):
            mu.reg_write(UC_ARM64_REG_X0 + i, a & 0xFFFFFFFFFFFFFFFF)
        mu.reg_write(UC_ARM64_REG_SP, STACK - 0x1000)
        mu.reg_write(UC_ARM64_REG_LR, RET_ADDR)
        mu.emu_start(addr, RET_ADDR, count=5_000_000)
        return mu.reg_read(UC_ARM64_REG_X0)

    # --- std::map<std::string,int> helpers (libc++ layout) ---
    def read_str(self, p):
        b0 = self.mu.mem_read(p, 1)[0]
        if b0 & 1 == 0:  # short
            return bytes(self.mu.mem_read(p + 1, b0 >> 1)).decode()
        size, = struct.unpack('<Q', self.mu.mem_read(p + 8, 8))
        data, = struct.unpack('<Q', self.mu.mem_read(p + 16, 8))
        return bytes(self.mu.mem_read(data, size)).decode()

    def map_nodes(self, mapaddr):
        root, = struct.unpack('<Q', self.mu.mem_read(mapaddr + 8, 8))
        out = {}

        def walk(n):
            if not n:
                return
            l, r = struct.unpack('<QQ', self.mu.mem_read(n, 16))
            walk(l)
            out[self.read_str(n + 0x20)] = n + 0x38
            walk(r)
        walk(root)
        return out


def q(mu, a):
    return struct.unpack('<Q', mu.mem_read(a, 8))[0]
