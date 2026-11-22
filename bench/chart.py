import json
import sys

AWSR = "#d9622b"
AWS = "#4a7bb7"


def medians(path):
    results = json.load(open(path))["results"]
    return {r["name"]: r["summary"]["time_wall_clock"]["median"] * 1000 for r in results}


def fmt(ms):
    return f"{ms:.1f} ms" if ms < 100 else f"{ms:.0f} ms"


def rows(times):
    names = [c.removeprefix("awsr ") for c in times if c.startswith("awsr ")]
    return [(n, times[f"awsr {n}"], times[f"aws {n}"]) for n in names]


def table(data, aws_version):
    lines = [f"| Command | awsr | aws {aws_version} | Speedup |", "|---|---:|---:|---:|"]
    for name, a, b in data:
        lines.append(f"| `{name}` | {fmt(a)} | {fmt(b)} | {b / a:.0f}× |")
    return "\n".join(lines)


def svg(data, aws_version, cpu):
    width, label_w, bar_x, row_h = 1100, 300, 320, 78
    top = 120
    height = top + row_h * len(data) + 50
    scale = (width - bar_x - 230) / max(b for _, _, b in data)
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="Helvetica, Arial, sans-serif">',
        f'<rect width="{width}" height="{height}" rx="16" fill="#fcfcfb"/>',
        '<text x="30" y="48" font-size="24" font-weight="bold" fill="#222">Median wall time (ms, lower is better)</text>',
        f'<rect x="30" y="70" width="22" height="22" rx="4" fill="{AWSR}"/><text x="62" y="88" font-size="19" fill="#222">awsr</text>',
        f'<rect x="150" y="70" width="22" height="22" rx="4" fill="{AWS}"/><text x="182" y="88" font-size="19" fill="#222">aws {aws_version}</text>',
    ]
    for i, (name, a, b) in enumerate(data):
        y = top + i * row_h
        out.append(f'<text x="{label_w}" y="{y + 34}" font-size="19" text-anchor="end" font-family="monospace" fill="#222">{name}</text>')
        for j, (ms, color) in enumerate(((a, AWSR), (b, AWS))):
            w = max(ms * scale, 4)
            by = y + j * 30
            out.append(f'<rect x="{bar_x}" y="{by}" width="{w:.1f}" height="26" rx="4" fill="{color}"/>')
            out.append(f'<text x="{bar_x + w + 8:.1f}" y="{by + 20}" font-size="19" fill="#222">{fmt(ms)}</text>')
        out.append(f'<text x="{width - 30}" y="{y + 50}" font-size="19" font-weight="bold" text-anchor="end" fill="#222">{b / a:.0f}×</text>')
    out.append(f'<text x="30" y="{height - 20}" font-size="15" fill="#555">hyperfine -N, local stub endpoint, {cpu}. Reproduce with bench/run.sh.</text>')
    out.append("</svg>")
    return "\n".join(out) + "\n"


if __name__ == "__main__":
    path, aws_version, cpu, svg_path = sys.argv[1:5]
    data = rows(medians(path))
    open(svg_path, "w").write(svg(data, aws_version, cpu))
    print(table(data, aws_version))
