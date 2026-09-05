import re, json
a = open('out-e/Body_2.umap','rb').read(); b = open('out-f/Body_2.umap','rb').read()
off = 311987
seg = a[off-40000:off]
strs = [(m.start()+off-40000, m.group().decode()) for m in re.finditer(rb'[ -~]{6,}', seg)]
print('strings in 40KB before:', [(p, s[:48]) for p, s in strs[-12:]])
print('between the two guids:', a[311983:312143].hex())
seg2 = a[off:off+40000]
strs2 = [(m.start()+off, m.group().decode()) for m in re.finditer(rb'[ -~]{6,}', seg2)]
print('strings in 40KB after:', [(p, s[:48]) for p, s in strs2[:8]])
for d in ('out-a','out-b','out-e','out-f'):
    j = json.load(open(f'{d}/body-2.cook.json'))
    print(d, 'timing', {k: round(v,3) for k,v in j['timing'].items()}, 'canonical', j.get('canonical_guids'))
