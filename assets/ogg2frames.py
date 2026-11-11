# Strip an Ogg Opus file into raw packets, written as 72-byte records (counter + 71 bytes) like `winsense mic`.
import sys
data=open(sys.argv[1],'rb').read(); pos=0; packets=[]; cur=b''
while pos < len(data):
    assert data[pos:pos+4]==b'OggS'
    nseg=data[pos+26]; lac=data[pos+27:pos+27+nseg]; body=pos+27+nseg
    for l in lac:
        cur+=data[body:body+l]; body+=l
        if l<255: packets.append(cur); cur=b''
    pos=body
audio=packets[2:]  # skip OpusHead, OpusTags
sizes=sorted(set(len(p) for p in audio)); tocs=sorted(set(p[0] for p in audio))
print(len(audio),'packets, sizes',sizes,'tocs',[hex(t) for t in tocs])
want=int(sys.argv[3]) if len(sys.argv)>3 else None
out=bytearray()
for i,p in enumerate(audio):
    if want: p=p[:want].ljust(want,b'\0')
    out+=bytes([i&255])+p if want==71 else p
open(sys.argv[2],'wb').write(out)
