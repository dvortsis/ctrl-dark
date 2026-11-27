#!/usr/bin/env python3
"""Rebuild Ctrl+Dark vector artwork and exact-resolution Windows icons offline."""
from pathlib import Path
import hashlib, io, json, logging, struct, sys
from PIL import Image, ImageDraw, ImageFont
import cairosvg

ROOT=Path(__file__).resolve().parent
logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(message)s', handlers=[logging.FileHandler(ROOT/'build.log', mode='w', encoding='utf-8'), logging.StreamHandler()])
SIZES=(16,20,24,32,40,48,64,96,128,256)
TRAY=(16,20,24,32,40,48,64)
# Hand reconstructed from concept #4. Coordinates follow its asymmetrical swept flame silhouette.
BODY='M181 10 C153 1 126 24 138 45 C151 40 170 57 157 73 C144 91 117 83 93 92 C53 105 32 136 34 177 C36 224 76 257 135 258 C136 235 162 220 189 207 C221 190 232 169 216 163 C213 172 207 173 205 167 C200 150 213 141 219 124 C234 83 203 54 179 44 C162 35 164 20 181 10Z'
LEFT='M65 81 C38 80 13 101 20 126 C22 137 31 143 43 146 C49 130 57 119 73 108 C51 112 46 95 65 81Z'
EMBER='M61 57 C72 35 98 28 112 48 C126 68 107 86 79 92 C94 77 98 65 88 57 C81 51 71 51 61 57Z'
LOW='M21 179 C2 196 13 225 38 233 C49 237 58 236 65 235 C51 226 44 215 40 202 C24 209 15 193 21 179Z'
RIGHT='M235 128 C258 149 254 189 233 214 C212 239 177 246 135 258 C142 236 160 226 185 213 C206 202 225 186 220 172 C217 165 210 169 206 177 C204 165 212 155 223 151 C231 147 235 137 235 128Z'
ACCENT='M200 70 C220 92 213 119 202 131 C195 139 194 146 193 155 C184 131 168 124 148 124 C180 115 203 98 200 70Z'
EYES=['M57 163 C74 165 93 182 94 201 C77 204 55 189 57 163Z','M165 165 C166 188 145 207 119 202 C123 184 144 168 165 165Z']
DEFS='''<defs><linearGradient id="body" x1="0" y1="0" x2=".85" y2="1"><stop stop-color="#30203F"/><stop offset=".58" stop-color="#1A1129"/><stop offset="1" stop-color="#100C1C"/></linearGradient><linearGradient id="violet" x1="0" y1="0" x2=".85" y2="1"><stop stop-color="#40206F"/><stop offset=".54" stop-color="#7837C6"/><stop offset="1" stop-color="#D8A3FF"/></linearGradient><linearGradient id="ribbon" x1=".9" y1="0" x2=".15" y2="1"><stop stop-color="#4D2587"/><stop offset=".62" stop-color="#9B50F2"/><stop offset="1" stop-color="#E2B0FF"/></linearGradient><linearGradient id="bg" x1="0" y1="0" x2="1" y2="1"><stop stop-color="#100C1C"/><stop offset="1" stop-color="#2A1750"/></linearGradient></defs>'''

def path(d,fill,extra=''): return f'<path d="{d}" fill="{fill}" {extra}/>'
def svg(content, w=256,h=256,view=None): return f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="{view or f"0 0 {w} {h}"}">{DEFS}{content}</svg>\n'
def mascot(state='active'):
    out=''.join(path(d,'url(#violet)') for d in (LEFT,EMBER,LOW))
    out+=path(BODY,'url(#body)','stroke="#A36DE0" stroke-width="3.6" stroke-linejoin="round"')+path(ACCENT,'url(#violet)')+path(RIGHT,'url(#ribbon)')
    out+=path('M135 258 C152 238 176 234 195 231 C220 226 238 212 246 193 C224 226 198 222 180 229 C158 238 146 245 135 258Z','#D9A5FF','opacity=".62"')
    if state=='sleeping':
        out+='<g fill="none" stroke="#F8EDFF" stroke-width="7" stroke-linecap="round"><path d="M57 177 Q75 194 93 183"/><path d="M120 183 Q144 194 164 177"/></g>'
    else:
        for eye in EYES:
            out+=path(eye,'#B66CF9','stroke="#B66CF9" stroke-width="9" opacity=".22" stroke-linejoin="round"')+path(eye,'#FFF8FF' if state!='alert' else '#FFD36D')
    return out

def group(content, x=0,y=0,scale=1): return f'<g transform="translate({x} {y}) scale({scale})">{content}</g>'
# Deliberately simplified 16px path, with larger eyes and substantial edge contrast.
SMALL='M10 1 C8.1 .4 7 2.1 8.1 3.5 C9.1 4.9 6.1 5.5 4.5 6.4 C2.4 7.6 1.7 9.2 2.2 11.5 C2.8 14 5 15 8.1 15 C11.9 15 14.5 12.9 14.6 9.7 C14.7 8.3 14.2 7 13.5 6.2 C13.7 8.1 12.3 8.9 12 7.2 C12 5.1 10.9 3.8 10 3.1 C9.1 2.4 9.1 1.6 10 1Z'
MID='M10.2 .9 C8.4 .2 7.2 1.8 8.1 3.3 C9.4 5.2 6.4 5.5 4.7 6.3 C2.4 7.4 1.4 9.1 2 11.6 C2.7 14.2 5.1 15.1 8.1 15.1 C11.8 15.1 14.6 12.6 14.6 9.7 C14.6 8.1 14.1 7 13.6 6.3 C13.8 8.2 12.5 9.1 12 7.4 C11.8 5.5 11 3.9 9.9 3.1 C9 2.3 9.2 1.5 10.2 .9Z'
SM_EYES=['M3.9 9.2 Q6.3 9.3 7.1 11.7 Q4.2 12 3.9 9.2Z','M11.8 9.2 Q9.4 9.3 8.6 11.7 Q11.4 12 11.8 9.2Z']
def app_small(n):
    out=path(SMALL if n==16 else MID,'#231536','stroke="#B681FF" stroke-width="1.35" stroke-linejoin="round"')
    out+=path('M1.5 8 C.6 6.4 2.1 4.7 4.2 4.9 C2.6 5.9 3 6.7 3.7 7Z','#C18AFF')
    out+=path('M14.4 8.5 C15.7 11.1 12.8 14.7 8.3 15.2 Q9.1 13.2 11.8 12.2 Q14.3 10.7 14.4 8.5Z','#BE86FF')
    if n>=24:
        out+=path('M3.5 3.8 Q4.6 1.9 5.8 3.1 Q6.6 4.2 4.5 4.7 Q5 3.5 3.5 3.8Z','#AD70F0')
    out+=''.join(path(d,'#FFFFFF') for d in SM_EYES)
    return svg(out,n,n,'0 0 16 16')

def tray_icon(n,theme,state):
    fg='#F7F5FC' if theme=='dark' else '#17111F'
    d=SMALL if n==16 else MID
    # Cut-outs are truly transparent, not a guessed taskbar background color.
    holes=' '.join(SM_EYES)
    if state=='sleeping': holes='M4 9.3 Q5.6 10.9 7.1 9.8 L7.2 10.8 Q5.6 12.1 3.9 10.4Z M8.6 9.8 Q10.1 10.9 11.7 9.3 L11.8 10.4 Q10.1 12.1 8.5 10.8Z'
    out=path(d+' '+holes,fg,'fill-rule="evenodd"')
    if state in ('disabled','alert'):
        out=group(out,-.35,.15,.76)
        # Knock out a clear separation between the silhouette and status badge.
        out='<defs><clipPath id="badge-gap"><path d="M0 0 H16 V16 H0 Z M8.0 12.1 A4.1 4.1 0 1 0 16.2 12.1 A4.1 4.1 0 1 0 8.0 12.1 Z" clip-rule="evenodd"/></clipPath></defs><g clip-path="url(#badge-gap)">'+out+'</g>'
        if state=='disabled':
            out+=f'<circle cx="12.1" cy="12.1" r="3.1" fill="none" stroke="{fg}" stroke-width="1.5"/><path d="M10.1 14.1 L14.1 10.1" stroke="{fg}" stroke-width="1.5" stroke-linecap="round"/>'
        else:
            out+=path('M12.3 8.1 L15.9 14.9 L8.7 14.9Z M11.75 10.5 L12.85 10.5 L12.85 12.7 L11.75 12.7Z M11.75 13.3 L12.85 13.3 L12.85 14.3 L11.75 14.3Z',fg,'fill-rule="evenodd"')
    return svg(out,n,n,'0 0 16 16')

def write(rel,data):
    p=ROOT/rel; p.parent.mkdir(parents=True,exist_ok=True); p.write_bytes(data if isinstance(data,bytes) else data.encode('utf-8')); return p

def render(source, rel, w,h=None):
    p=ROOT/rel;p.parent.mkdir(parents=True,exist_ok=True)
    cairosvg.svg2png(bytestring=source.encode(),write_to=str(p),output_width=w,output_height=h or w)
    with Image.open(p) as im: assert im.size==(w,h or w) and im.mode in ('RGB','RGBA'), str(p)
    return p

def ico(rel, frames):
    # Preserve exact hand-authored frame bytes, rather than resizing a single master in Pillow.
    data=[(n,p.read_bytes()) for n,p in frames]; offset=6+16*len(data); entries=[]
    for n,b in data:
        entries.append(struct.pack('<BBBBHHII',n%256,n%256,0,0,1,32,len(b),offset));offset+=len(b)
    p=write(rel,struct.pack('<HHH',0,1,len(data))+b''.join(entries)+b''.join(b for n,b in data))
    raw=p.read_bytes()
    for i,(n,b) in enumerate(data):
        w,h,_,_,_,depth,length,start=struct.unpack_from('<BBBBHHII',raw,6+i*16)
        assert (w or 256,h or 256,depth)==(n,n,32) and raw[start:start+length]==b
        with Image.open(io.BytesIO(raw[start:start+length])) as im: assert im.size==(n,n);im.verify()
    with Image.open(p) as im: assert im.ico.sizes()=={(n,n) for n,_ in data}
    return p

def build():
    master=svg(group(mascot(),10,1,.91),256,256)
    write('source/mascot-master.svg',master)
    frames=[]
    for n in SIZES:
        source=app_small(n) if n<=40 else master
        write(f'source/icons/CtrlDark-{n}.svg',source)
        frames.append((n,render(source,f'icons/CtrlDark-{n}.png',n)))
    ico('icons/CtrlDark.ico',frames)
    # Taskbar and executable use the same size-specific color icon by design.
    write('icons/taskbar.ico',(ROOT/'icons/CtrlDark.ico').read_bytes())
    for theme in ('dark','light'):
        for state in ('active','sleeping','disabled','alert'):
            frames=[]
            for n in TRAY:
                source=tray_icon(n,theme,state)
                write(f'source/tray/{theme}-{state}-{n}.svg',source)
                frames.append((n,render(source,f'tray/{theme}-{state}-{n}.png',n)))
            ico(f'tray/{theme}-{state}.ico',frames)
        write(f'tray/{theme}-taskbar.ico',(ROOT/f'tray/{theme}-active.ico').read_bytes())
    for state in ('active','sleeping','disabled','alert'):
        write(f'tray/{state}.ico',(ROOT/f'tray/dark-{state}.ico').read_bytes())
    # Marketing poses keep exactly the same master character geometry.
    poses={
      'original-04':group(mascot(),10,1,.91),
      'flame':group(mascot(),10,1,.91)+'<path d="M226 42 V60 M217 51 H235 M38 33 V45 M32 39 H44" stroke="#CFA1FF" stroke-width="4" stroke-linecap="round"/>',
      'sleeping':group(mascot('sleeping'),10,9,.85)+'<g fill="#B681FF" font-family="DejaVu Sans" font-weight="bold"><text x="199" y="53" font-size="21">z</text><text x="219" y="28" font-size="27">Z</text></g>',
      'alert':group(mascot('alert'),10,8,.85)+'<circle cx="220" cy="48" r="22" fill="#FFCD68"/><path d="M220 36 V49 M220 57 V58" stroke="#271638" stroke-width="6" stroke-linecap="round"/>',
      'monitor':group(mascot(),28,0,.76)+'<rect x="36" y="155" width="184" height="76" rx="9" fill="#241A36" stroke="#A672EF" stroke-width="5"/><rect x="44" y="163" width="168" height="60" rx="4" fill="#100C1B"/><path d="M128 231 V245 M103 245 H153" fill="none" stroke="#A672EF" stroke-width="7" stroke-linecap="round"/>',
      'hotkey':group(mascot(),23,1,.80)+'<rect x="62" y="177" width="132" height="65" rx="14" fill="#291B40" stroke="#B681FF" stroke-width="5"/><text x="128" y="217" text-anchor="middle" font-family="DejaVu Sans" font-weight="bold" font-size="24" fill="#FFF8FF">Ctrl</text>',
      'moon':'<circle cx="132" cy="132" r="113" fill="#B681FF" opacity=".22"/>'+path('M211 33 A110 110 0 1 0 227 209 A103 103 0 0 1 211 33Z','#CFA1FF')+group(mascot(),24,5,.81),
      'minimal':group(app_small(32).split('</defs>')[1].split('</svg>')[0],12,12,14.5),
      'white-flame':group(path(MID+' '+' '.join(SM_EYES),'#F7F5FC','fill-rule="evenodd"'),8,8,15),
      'dark-flame':group(path(MID+' '+' '.join(SM_EYES),'#17111F','fill-rule="evenodd"'),8,8,15),
    }
    for name,art in poses.items():
        source=svg(art);write(f'source/mascots/{name}.svg',source);render(source,f'mascots/{name}.png',512)
    avatar=svg('<rect x="8" y="8" width="240" height="240" rx="56" fill="url(#bg)" stroke="#A16CE6" stroke-width="2"/>'+group(mascot(),22,10,.83))
    write('source/github/avatar.svg',avatar);render(avatar,'github/avatar.png',512)
    for pose in ('monitor','hotkey','moon','sleeping','alert'):
        source=svg(poses[pose]);write(f'source/github/readme-{pose}.svg',source);render(source,f'github/readme-{pose}.png',512)
    for name,w,h in [('social-preview',1280,640),('readme-banner',1280,320)]:
        s=1.65 if h==640 else .98; x=90 if h==640 else 102; y=90 if h==640 else 27; tx=550 if h==640 else 400; ty=290 if h==640 else 139
        font=102 if h==640 else 80; sub=29 if h==640 else 24
        art=f'<rect width="{w}" height="{h}" fill="url(#bg)"/><circle cx="{x+128*s}" cy="{y+130*s}" r="{130*s}" fill="#7542B4" opacity=".12"/><circle cx="{x+128*s}" cy="{y+130*s}" r="{111*s}" fill="none" stroke="#9B64D9" stroke-width="1.5" opacity=".32"/>'+group(mascot(),x,y,s)+f'<g font-family="DejaVu Sans"><text x="{tx}" y="{ty}" font-size="{font}" font-weight="bold" fill="#FFFFFF">Ctrl+<tspan fill="#C79AFF">Dark</tspan></text><text x="{tx+3}" y="{ty+58 if h==640 else ty+44}" fill="#E4D8F6" font-size="{sub}">Turn off your monitors with a hotkey.</text></g>'
        source=svg(art,w,h);write(f'source/github/{name}.svg',source);render(source,f'github/{name}.png',w,h)
    # All states at native size, with enough clearance for every frame.
    im=Image.new('RGB',(1000,830),'#F5F3F9');d=ImageDraw.Draw(im)
    font=ImageFont.truetype('DejaVuSans.ttf',16); small=ImageFont.truetype('DejaVuSans.ttf',13)
    d.text((24,16),'Ctrl+Dark | Native-size Windows icon check',font=font,fill='#181323')
    for j,(theme,bg,fg) in enumerate([('dark','#202020','#FFFFFF'),('light','#F3F3F3','#181323')]):
        y=53+j*385;d.rounded_rectangle((20,y,980,y+370),radius=9,fill=bg,outline='#9C95AA');d.text((38,y+10),f'{theme.title()} taskbar',font=font,fill=fg)
        for i,n in enumerate((16,20,24,32,40,48,64)):
            xx=270+i*95;d.text((xx+25,y+9),str(n),font=small,fill=fg)
            for k,state in enumerate(('app','active','sleeping','disabled','alert')):
                yy=y+30+k*68
                if i==0:d.text((38,yy+24),state.title(),font=small,fill=fg)
                p=ROOT/(f'icons/CtrlDark-{n}.png' if state=='app' else f'tray/{theme}-{state}-{n}.png')
                with Image.open(p) as icon: im.paste(icon,(xx+(64-n)//2,yy+(64-n)//2),icon)
    (ROOT/'preview').mkdir(exist_ok=True);im.save(ROOT/'preview/icon-size-check.png')
    # Contact sheet on alternating light and dark backgrounds.
    im=Image.new('RGB',(1000,475),'#F5F3F9');d=ImageDraw.Draw(im)
    for i,name in enumerate(poses):
        x=(i%5)*200;y=(i//5)*235;dark=(name=='white-flame' or (i%2==1 and name!='dark-flame'));bg='#202020' if dark else '#F5F3F9';fg='#FFFFFF' if dark else '#181323'
        d.rectangle((x,y,x+199,y+234),fill=bg)
        with Image.open(ROOT/f'mascots/{name}.png') as icon:
            icon=icon.resize((180,180),Image.Resampling.LANCZOS);im.paste(icon,(x+10,y+8),icon)
        d.text((x+12,y+202),name,font=small,fill=fg)
    im.save(ROOT/'preview/mascot-contact-sheet.png')
    # Enlarged actual pixels for explicit 16px QA.
    im=Image.new('RGB',(820,360),'#F5F3F9');d=ImageDraw.Draw(im)
    for j,(theme,bg,fg) in enumerate([('dark','#202020','#FFFFFF'),('light','#F3F3F3','#181323')]):
        y=j*180;d.rectangle((0,y,820,y+179),fill=bg)
        for i,state in enumerate(('app','active','sleeping','disabled','alert')):
            x=16+i*162;d.text((x,y+7),state.title(),font=small,fill=fg)
            with Image.open(ROOT/(f'icons/CtrlDark-16.png' if state=='app' else f'tray/{theme}-{state}-16.png')) as icon:
                icon=icon.resize((128,128),Image.Resampling.NEAREST);im.paste(icon,(x,y+30),icon)
    im.save(ROOT/'preview/16px-pixel-check.png')
    # Verify decoded alpha and no boundary contamination for transparent square assets.
    pngs=[p for folder in ('icons','tray','mascots','github','preview') for p in (ROOT/folder).glob('*.png')];icos=[p for folder in ('icons','tray') for p in (ROOT/folder).glob('*.ico')]
    for p in pngs:
        with Image.open(p) as im:
            im.load()
            if p.parent.name in ('icons','tray','mascots'):
                assert im.mode=='RGBA' and im.getchannel('A').getextrema()==(0,255),p
                assert all(im.getpixel(xy)[3]==0 for xy in [(0,0),(im.width-1,0),(0,im.height-1),(im.width-1,im.height-1)]),p
    for p in icos:
        with Image.open(p) as im: assert im.ico.sizes()=={(n,n) for n in (SIZES if p.parent.name=='icons' else TRAY)},p
    manifest={ 'version':'2.0.0','png_count':len(pngs),'ico_count':len(icos),'files':[] }
    for p in sorted(ROOT.rglob('*')):
        if p.is_file() and p.name not in ('manifest.json','build.log') and not any(part.startswith('.') or part=='__pycache__' for part in p.relative_to(ROOT).parts):
            manifest['files'].append({'path':p.relative_to(ROOT).as_posix(),'sha256':hashlib.sha256(p.read_bytes()).hexdigest(),'bytes':p.stat().st_size})
    write('manifest.json',json.dumps(manifest,indent=2)+'\n');logging.info('Verified %s PNGs and %s ICOs.',len(pngs),len(icos))
if __name__=='__main__':
    try: build()
    except Exception: logging.exception('Asset build failed');sys.exit(1)
