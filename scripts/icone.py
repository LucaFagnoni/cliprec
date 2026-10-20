"""Genera le icone di ClipRec da assets/cliprec.png (quadrato, con trasparenza).

Uscite in assets/icon/: PNG per misura, cliprec.ico (installer, collegamenti), cliprec.res (risorsa
icona per gli eseguibili, passata al linker MSVC dai build.rs: niente rc.exe) e tray-32.rgba (pixel
grezzi per l'icona della tray). Ridimensiona con ffmpeg (lanczos) da third_party/ffmpeg.

Uso: python -I scripts/icone.py
"""
import pathlib
import struct
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent.parent
FFMPEG = ROOT / 'third_party' / 'ffmpeg' / 'bin' / 'ffmpeg.exe'
SRC = ROOT / 'assets' / 'cliprec.png'
OUT = ROOT / 'assets' / 'icon'
SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]


def scaled(size, fmt):
    args = [str(FFMPEG), '-v', 'error', '-i', str(SRC), '-vf', f'scale={size}:{size}:flags=lanczos', '-pix_fmt', 'rgba']
    args += ['-f', 'image2pipe', '-vcodec', 'png', '-'] if fmt == 'png' else ['-f', 'rawvideo', '-']
    return subprocess.run(args, check=True, capture_output=True).stdout


def ico(images):
    head = struct.pack('<HHH', 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, data = b'', b''
    for size, png in images:
        entries += struct.pack('<BBBBHHII', size % 256, size % 256, 0, 0, 1, 32, len(png), offset + len(data))
        data += png
    return head + entries + data


def res(images):
    """File .res con un RT_ICON per misura e il gruppo RT_GROUP_ICON 1 (l'icona dell'eseguibile)."""

    def entry(rtype, name, flags, payload):
        header = struct.pack('<II', len(payload), 32) + struct.pack('<HH', 0xFFFF, rtype) + struct.pack('<HH', 0xFFFF, name)
        header += struct.pack('<IHHII', 0, flags, 0x0409, 0, 0)
        return header + payload + b'\0' * (-len(payload) % 4)

    out = struct.pack('<II', 0, 32) + struct.pack('<HHHH', 0xFFFF, 0, 0xFFFF, 0) + struct.pack('<IHHII', 0, 0, 0, 0, 0)
    group = struct.pack('<HHH', 0, 1, len(images))
    for i, (size, png) in enumerate(images, start=1):
        out += entry(3, i, 0x1010, png)
        group += struct.pack('<BBBBHHIH', size % 256, size % 256, 0, 0, 1, 32, len(png), i)
    return out + entry(14, 1, 0x1030, group)


def main():
    OUT.mkdir(parents=True, exist_ok=True)
    images = [(s, scaled(s, 'png')) for s in SIZES]
    for size, png in images:
        (OUT / f'cliprec-{size}.png').write_bytes(png)
    (OUT / 'cliprec.ico').write_bytes(ico(images))
    (OUT / 'cliprec.res').write_bytes(res(images))
    (OUT / 'tray-32.rgba').write_bytes(scaled(32, 'raw'))
    print('icone in', OUT)


main()
