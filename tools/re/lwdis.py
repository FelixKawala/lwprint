import sys
from elftools.elf.elffile import ELFFile
from capstone import *
import os
path = os.environ.get('LWPRINT_SO', 'libLWPrint.so')  # arm64-v8a build from the app's split APK
f=open(path,'rb'); elf=ELFFile(f)
data=open(path,'rb').read()
# map vaddr->offset via segments
segs=[(s['p_vaddr'],s['p_offset'],s['p_filesz']) for s in elf.iter_segments() if s['p_type']=='PT_LOAD']
def off(va):
    for v,o,sz in segs:
        if v<=va<v+sz: return va-v+o
syms={}
for sec in elf.iter_sections():
    if sec.name in ('.dynsym',):
        for s in sec.iter_symbols():
            if s['st_value']: syms[s['st_value']]=s.name
# plt relocs
plt={}
rela=elf.get_section_by_name('.rela.plt'); dsym=elf.get_section_by_name('.dynsym')
for r in rela.iter_relocations():
    plt[r['r_offset']]=dsym.get_symbol(r['r_info_sym']).name
md=Cs(CS_ARCH_ARM64,CS_MODE_ARM)
def dis(va,n):
    o=off(va)
    for i in md.disasm(data[o:o+n*4],va):
        c=i.op_str
        print(f"{i.address:08x}: {i.mnemonic:8s} {c}" + (f"   <{syms[int(c[1:],16)]}>" if i.mnemonic=='bl' and c.startswith('#') and int(c[1:],16) in syms else ""))
        if i.mnemonic=='ret' and n<0: break
if __name__=='__main__':
    dis(int(sys.argv[1],16),int(sys.argv[2]))

def plt_target(va):
    o=off(va); ins=list(md.disasm(data[o:o+16],va))
    # adrp x16,page; ldr x17,[x16,#imm]
    page=int(ins[0].op_str.split('#')[1],16)
    imm=int(ins[1].op_str.split('#')[1].rstrip(']'),16)
    return plt.get(page+imm,'?')
