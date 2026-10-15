"""Example: run CommandFilter::makePageEnvironmentCommand from libLWPrint.so under Unicorn.
Usage: LWPRINT_SO=/path/to/libLWPrint.so python3 example_page_env.py
"""
from emu import *
def page(h,w,m,lvl=1,params=None,status=None):
    e=Emu(); obj=e.malloc(0x100)
    e.call('_ZN13CommandFilterC1EP7_JNIEnv', obj, JNI)
    e.call('_ZN13CommandFilter25makeJobEnvironmentCommandEPc', obj, SCRATCH)
    e.call('_ZN13CommandFilter26makePageEnvironmentCommandEPciiii', obj, SCRATCH, h,w,m,lvl)
    nodes=e.map_nodes(obj+0x10); snodes=e.map_nodes(obj+0x28)
    for k,v in (params or {}).items(): e.mu.mem_write(nodes[k], struct.pack('<i', v))
    for k,v in (status or {}).items(): e.mu.mem_write(snodes[k], struct.pack('<i', v))
    n=e.call('_ZN13CommandFilter26makePageEnvironmentCommandEPciiii', obj, SCRATCH, h,w,m,lvl)
    return bytes(e.mu.mem_read(SCRATCH,n)).hex(' '), list(nodes)+list(snodes), set(e.log)
print(page(500,64,0))
for args in [(500,64,0),(1000,64,0),(500,128,0),(500,64,10),(300,96,20)]:
    print(args, page(*args)[0])
