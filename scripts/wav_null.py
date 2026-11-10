import math
import struct
import sys


def read_wav(path):
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError(f"{path}: not a WAV file")
    pos, fmt, body = 12, None, None
    while pos + 8 <= len(data):
        chunk_id, size = data[pos:pos + 4], struct.unpack("<I", data[pos + 4:pos + 8])[0]
        chunk = data[pos + 8:pos + 8 + size]
        if chunk_id == b"fmt ":
            fmt = chunk
        elif chunk_id == b"data":
            body = chunk
        pos += 8 + size + (size & 1)
    tag, channels, rate, _, _, bits = struct.unpack("<HHIIHH", fmt[:16])
    if tag == 0xFFFE:
        tag = struct.unpack("<H", fmt[24:26])[0]
    width = bits // 8
    count = len(body) // width
    if tag == 3 and bits == 32:
        samples = list(struct.unpack(f"<{count}f", body[:count * 4]))
    elif tag == 3 and bits == 64:
        samples = list(struct.unpack(f"<{count}d", body[:count * 8]))
    elif tag == 1 and bits == 16:
        samples = [s / 32768.0 for s in struct.unpack(f"<{count}h", body[:count * 2])]
    elif tag == 1 and bits == 32:
        samples = [s / 2147483648.0 for s in struct.unpack(f"<{count}i", body[:count * 4])]
    elif tag == 1 and bits == 24:
        samples = [int.from_bytes(body[i:i + 3], "little", signed=True) / 8388608.0 for i in range(0, count * 3, 3)]
    else:
        raise ValueError(f"{path}: unsupported format tag={tag} bits={bits}")
    return {"path": path, "tag": tag, "bits": bits, "channels": channels, "rate": rate, "samples": samples}


def db(value):
    return "-inf" if value <= 0 else f"{20 * math.log10(value):.1f}"


def describe(w):
    s = w["samples"]
    frames = len(s) // w["channels"]
    peak = max(map(abs, s), default=0.0)
    rms = math.sqrt(sum(x * x for x in s) / len(s)) if s else 0.0
    print(f"{w['path']}\n  format tag={w['tag']} bits={w['bits']} ch={w['channels']} rate={w['rate']} "
          f"frames={frames} ({frames / w['rate']:.3f} s) peak={db(peak)} dBFS rms={db(rms)} dBFS")


def compare(a, b, offset=0):
    ch = a["channels"]
    x, y = a["samples"], b["samples"]
    start_x = max(0, -offset) * ch
    start_y = max(0, offset) * ch
    n = min(len(x) - start_x, len(y) - start_y)
    peak, energy, different, first = 0.0, 0.0, 0, None
    for i in range(n):
        d = x[start_x + i] - y[start_y + i]
        if d != 0.0:
            different += 1
            if first is None:
                first = i // ch
        peak = max(peak, abs(d))
        energy += d * d
    rms = math.sqrt(energy / n) if n else 0.0
    return {"n": n, "peak": peak, "rms": rms, "different": different, "first": first}


def best_offset(a, b, limit):
    ch = a["channels"]
    x = a["samples"][::ch]
    y = b["samples"][::ch]
    n = min(len(x), len(y)) - limit * 2
    if n <= 0:
        return 0, 0.0
    step = max(1, n // 20000)
    best, best_energy = 0, None
    for offset in range(-limit, limit + 1):
        energy = sum((x[limit + i] - y[limit + i + offset]) ** 2 for i in range(0, n, step))
        if best_energy is None or energy < best_energy:
            best, best_energy = offset, energy
    return best, best_energy


def report(label, a, b):
    print(f"\n=== {label} ===")
    if a["channels"] != b["channels"] or a["rate"] != b["rate"]:
        print("  different channels or rate, not comparable")
        return
    r = compare(a, b)
    print(f"  no shift: samples compared={r['n']} different={r['different']} first_different_frame={r['first']}")
    print(f"  residual peak={db(r['peak'])} dBFS rms={db(r['rms'])} dBFS")
    offset, _ = best_offset(a, b, 2000)
    print(f"  best alignment: b is shifted by {offset} frames relative to a")
    if offset != 0:
        r = compare(a, b, offset)
        print(f"  after alignment: different={r['different']} residual peak={db(r['peak'])} dBFS rms={db(r['rms'])} dBFS")


def main():
    dry, ott, vault = (read_wav(p) for p in sys.argv[1:4])
    for w in (dry, ott, vault):
        describe(w)
    report("OTT vs OTT vault (the null test)", ott, vault)
    report("control: dry vs OTT (shows OTT changes the sound)", dry, ott)
    report("control: dry vs OTT vault (the bridge must change it the same way)", dry, vault)


main()
