# Strip an Ogg Opus file into its raw audio packets. Run: python ogg2frames.py <in.ogg> <out.bin> [size]
# Three output modes, by [size]:
#   none: the packets back to back, as they are.
#   71:   72-byte records (a counter byte + the packet padded to 71), like `winsense mic`.
#   else: each packet padded with zeros to [size] bytes (the clips use 200).
# A packet longer than [size] stops the run: cutting it would corrupt the clip.
import sys
if len(sys.argv) not in (3, 4):
    sys.exit('usage: python ogg2frames.py <in.ogg> <out.bin> [size]')
data=open(sys.argv[1],'rb').read(); pos=0; packets=[]; cur=b''
while pos < len(data):
    if data[pos:pos+4]!=b'OggS':
        sys.exit(f'{sys.argv[1]}: no Ogg page at byte {pos}; is this an Ogg Opus file?')
    nseg=data[pos+26]; lac=data[pos+27:pos+27+nseg]; body=pos+27+nseg
    for l in lac:
        cur+=data[body:body+l]; body+=l
        if l<255: packets.append(cur); cur=b''
    pos=body
audio=[p for p in packets[2:] if p]  # skip OpusHead, OpusTags and empty packets
if not audio: sys.exit(f'{sys.argv[1]}: no audio packets')
sizes=sorted(set(len(p) for p in audio)); tocs=sorted(set(p[0] for p in audio))
print(len(audio),'packets, sizes',sizes,'tocs',[hex(t) for t in tocs])
want=int(sys.argv[3]) if len(sys.argv)>3 else None
out=bytearray()
for i,p in enumerate(audio):
    if want:
        if len(p)>want: sys.exit(f'packet {i} is {len(p)} bytes, more than {want}: encode at a lower bitrate')
        p=p.ljust(want,b'\0')
    out+=bytes([i&255])+p if want==71 else p
open(sys.argv[2],'wb').write(out)
