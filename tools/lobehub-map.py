"""Build crates/myuri/assets/lobehub.json from @lobehub/icons.

The package's modelConfig and providerConfig say which icon goes with which
model or provider name (keywords, some of them regular expressions). This
script turns them into plain data and checks every icon against the static
SVG package, noting whether a colored variant exists.

    python tools/lobehub-map.py
"""
import json
import re
import time
import urllib.request
from pathlib import Path

ICONS = "6.18.0"
STATIC = "1.104.0"
OUT = Path(__file__).resolve().parent.parent / "crates" / "myuri" / "assets" / "lobehub.json"


def fetch(url: str) -> str:
    # unpkg drops connections now and then.
    for attempt in range(4):
        try:
            with urllib.request.urlopen(url, timeout=60) as reply:
                return reply.read().decode("utf-8")
        except OSError:
            if attempt == 3:
                raise
            time.sleep(2)
    raise RuntimeError("unreachable")


def static_files() -> set[str]:
    meta = json.loads(fetch(f"https://unpkg.com/@lobehub/icons-static-svg@{STATIC}/icons/?meta"))
    return {entry["path"].rsplit("/", 1)[-1] for entry in meta["files"]}


def rules(name: str, files: set[str]) -> list[dict]:
    source = fetch(f"https://unpkg.com/@lobehub/icons@{ICONS}/es/features/{name}.mjs")
    imports = dict(re.findall(r'import (\w+(?:\$\d+)?) from "\.\./([^/]+)/index\.mjs"', source))
    found = []
    for icon, body in re.findall(r"Icon:\s*(\w+(?:\$\d+)?),[^{}]*?keywords:\s*\[(.*?)\]", source, re.S):
        folder = imports.get(icon)
        if not folder:
            continue
        base = folder.lower()
        if f"{base}.svg" not in files and f"{base}-color.svg" not in files:
            continue
        keywords = [json.loads(word) for word in re.findall(r'"(?:[^"\\]|\\.)*"', body)]
        found.append({"keywords": keywords, "icon": base, "color": f"{base}-color.svg" in files})
    return found


def main() -> None:
    files = static_files()
    data = {
        "icons_version": ICONS,
        "static_version": STATIC,
        # Models match when a keyword, read as a case-insensitive regular
        # expression, is found in the name; providers when a keyword equals
        # the lowercased name. That is how @lobehub/icons itself matches.
        "models": rules("modelConfig", files),
        "providers": rules("providerConfig", files),
        "available": sorted(name[:-4] for name in files if name.endswith(".svg")),
    }
    OUT.write_text(json.dumps(data, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    print(f"{OUT.name}: {len(data['models'])} model rules, {len(data['providers'])} provider rules, "
          f"{len(data['available'])} icon files")


if __name__ == "__main__":
    main()
