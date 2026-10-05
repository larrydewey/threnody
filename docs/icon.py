# Generates the app icon (adaptive layers, monochrome, notification) and docs/icon.svg: python3 docs/icon.py .
import os, sys
OUT = sys.argv[1]  # repo root

def rrect(x, y, w, h, r):
    return (f"M{x+r},{y} H{x+w-r} A{r},{r} 0 0 1 {x+w},{y+r} V{y+h-r} A{r},{r} 0 0 1 {x+w-r},{y+h} "
            f"H{x+r} A{r},{r} 0 0 1 {x},{y+h-r} V{y+r} A{r},{r} 0 0 1 {x+r},{y} Z")

def bubble(cx, cy, s):
    """One outline: a rounded bubble whose lower-left corner flows into
    the tail (no seam). Everything stays inside the adaptive icon's safe
    circle (radius 33 around the centre)."""
    w, h, r = 56, 44, 16
    x, y = cx - w/2, cy - h/2
    b = y + h
    body = (f"M{x+r},{y} H{x+w-r} A{r},{r} 0 0 1 {x+w},{y+r} V{b-r} A{r},{r} 0 0 1 {x+w-r},{b} "
            f"H{x+18} C{x+14},{b+4} {x+10},{b+6.5} {x+5},{b+7} "
            f"C{x+7},{b+4.5} {x+7.4},{b+1.5} {x+7.2},{b-1.6} "
            f"A{r},{r} 0 0 1 {x},{b-r} V{y+r} A{r},{r} 0 0 1 {x+r},{y} Z")
    return body, "", (x, y, w, h)

def bars(cx, cy, s):
    heights = [10, 19, 27, 17, 9]
    bw, gap = 4.6*s, 4.2*s
    total = len(heights)*bw + (len(heights)-1)*gap
    x0 = cx - total/2
    out = []
    for i, hh in enumerate(heights):
        hh *= s
        x = x0 + i*(bw+gap)
        out.append(rrect(x, cy - hh/2, bw, hh, bw/2))
    return out

def fmt(p):
    import re
    return re.sub(r"(\d+\.\d{3})\d+", r"\1", p)

cx, cy = 54, 51
body, tail, (bx, by, bw, bh) = bubble(cx, cy, 1.0)
bar_paths = bars(cx, cy, 1.0)

res = os.path.join(OUT, "apps/android/app/src/main/res")
os.makedirs(os.path.join(res, "mipmap-anydpi-v26"), exist_ok=True)
VEC = 'xmlns:android="http://schemas.android.com/apk/res/android"'
AAPT = 'xmlns:aapt="http://schemas.android.com/aapt"'

open(os.path.join(res, "drawable/ic_launcher_background.xml"), "w").write(f'''<?xml version="1.0" encoding="utf-8"?>
<!-- Deep indigo to violet, from the app's accent. -->
<vector {VEC} {AAPT}
    android:width="108dp" android:height="108dp"
    android:viewportWidth="108" android:viewportHeight="108">
    <path android:pathData="M0,0h108v108h-108z">
        <aapt:attr name="android:fillColor">
            <gradient android:type="linear"
                android:startX="0" android:startY="0" android:endX="108" android:endY="108">
                <item android:offset="0" android:color="#FF23246E" />
                <item android:offset="0.55" android:color="#FF4A4FC4" />
                <item android:offset="1" android:color="#FF8A6BE0" />
            </gradient>
        </aapt:attr>
    </path>
</vector>
''')

bars_xml = "\n".join(f'    <path android:fillColor="#FF3B3FA8" android:pathData="{fmt(p)}" />' for p in bar_paths)
open(os.path.join(res, "drawable/ic_launcher_foreground.xml"), "w").write(f'''<?xml version="1.0" encoding="utf-8"?>
<!-- A speech bubble holding a waveform: messages, and the song a threnody is. -->
<vector {VEC}
    android:width="108dp" android:height="108dp"
    android:viewportWidth="108" android:viewportHeight="108">
    <path android:fillColor="#33000000" android:pathData="{fmt(body)} {fmt(tail)}"
        android:translateY="1.5" />
    <path android:fillColor="#FFFFFFFF" android:pathData="{fmt(body)} {fmt(tail)}" />
{bars_xml}
  </group>
</vector>
'''.replace('android:translateY="1.5" />', '/>').replace(
    f'<path android:fillColor="#33000000" android:pathData="{fmt(body)} {fmt(tail)}"\n        />',
    f'<group android:translateY="1.5">\n        <path android:fillColor="#33000000" android:pathData="{fmt(body)} {fmt(tail)}" />\n    </group>'))

# One shape, the bars cut out: for themed icons and notifications.
mono = " ".join([fmt(body), fmt(tail)] + [fmt(p) for p in bar_paths])
open(os.path.join(res, "drawable/ic_launcher_monochrome.xml"), "w").write(f'''<?xml version="1.0" encoding="utf-8"?>
<vector {VEC}
    android:width="108dp" android:height="108dp"
    android:viewportWidth="108" android:viewportHeight="108">
    <group android:scaleX="0.9" android:scaleY="0.9" android:pivotX="54" android:pivotY="54">
        <path android:fillColor="#FFFFFFFF" android:fillType="evenOdd" android:pathData="{mono}" />
    </group>
</vector>
''')
# Notification icon: the same shape on a 24dp canvas (bubble spans ~30..78).
open(os.path.join(res, "drawable/ic_notification.xml"), "w").write(f'''<?xml version="1.0" encoding="utf-8"?>
<vector {VEC}
    android:width="24dp" android:height="24dp"
    android:viewportWidth="56" android:viewportHeight="56">
    <group android:translateX="-26" android:translateY="-25">
        <path android:fillColor="#FFFFFFFF" android:fillType="evenOdd" android:pathData="{mono}" />
    </group>
</vector>
''')
for name in ["ic_launcher", "ic_launcher_round"]:
    open(os.path.join(res, f"mipmap-anydpi-v26/{name}.xml"), "w").write('''<?xml version="1.0" encoding="utf-8"?>
<adaptive-icon xmlns:android="http://schemas.android.com/apk/res/android">
    <background android:drawable="@drawable/ic_launcher_background" />
    <foreground android:drawable="@drawable/ic_launcher_foreground" />
    <monochrome android:drawable="@drawable/ic_launcher_monochrome" />
</adaptive-icon>
''')

# The same art as SVG, for the README and previews.
bars_svg = "\n".join(f'    <path fill="#3B3FA8" d="{fmt(p)}"/>' for p in bar_paths)
svg = f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 108 108" width="512" height="512">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#23246E"/>
      <stop offset="0.55" stop-color="#4A4FC4"/>
      <stop offset="1" stop-color="#8A6BE0"/>
    </linearGradient>
    <clipPath id="mask"><rect width="108" height="108" rx="24"/></clipPath>
  </defs>
  <g clip-path="url(#mask)">
    <rect width="108" height="108" fill="url(#bg)"/>
    <g transform="translate(54,54) scale(0.9) translate(-54,-54)">
    <path fill="#000" fill-opacity="0.2" transform="translate(0,1.5)" d="{fmt(body)} {fmt(tail)}"/>
    <path fill="#fff" d="{fmt(body)} {fmt(tail)}"/>
{bars_svg}
    </g>
  </g>
</svg>
'''
os.makedirs(os.path.join(OUT, "docs"), exist_ok=True)
open(os.path.join(OUT, "docs/icon.svg"), "w").write(svg)
print("bubble box", bx, by, bw, bh)

p = os.path.join(res, "drawable/ic_launcher_foreground.xml")
s = open(p).read()
if 'scaleX="0.9"' not in s:
    h = s.index('android:viewportHeight="108">') + len('android:viewportHeight="108">')
    body = s[h:s.index('</vector>')]
    s = s[:h] + '\n  <group android:scaleX="0.9" android:scaleY="0.9" android:pivotX="54" android:pivotY="54">' + body.replace('\n    ', '\n      ').rstrip() + '\n  </group>\n</vector>\n'
    open(p, "w").write(s)
