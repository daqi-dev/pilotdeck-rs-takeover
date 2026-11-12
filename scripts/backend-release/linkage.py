"""Inspect native dependency closure without executing the inspected binaries."""
import re
from common import binary_target, run


def inspect(root, target):
    files = [p for p in root.rglob("*") if p.is_file() and not p.is_symlink() and binary_target(p)]
    bundled = {p.name for p in root.rglob("*") if p.is_file()}
    result = []
    system_linux = re.compile(r"^(lib(c|m|gcc_s|stdc\+\+|pthread|dl|rt|resolv|util|crypt)\.so(?:\..+)?|ld-linux[^/]+)$")
    for binary in sorted(files):
        if binary_target(binary) != target:
            raise ValueError(f"mixed architecture: {binary.relative_to(root)}")
        if "linux" in target:
            raw = run("readelf", "-d", str(binary))
            libraries = re.findall(r"\(NEEDED\).*?\[(.*?)\]", raw)
            search = re.findall(r"\((?:RPATH|RUNPATH)\).*?\[(.*?)\]", raw)
            for item in libraries:
                if "/" in item or (item not in bundled and not system_linux.fullmatch(item)):
                    raise ValueError(f"unbundled non-system dependency: {binary.name}: {item}")
            for entry in search:
                for item in entry.split(":"):
                    if not item.startswith(("$ORIGIN", "${ORIGIN}")):
                        raise ValueError(f"non-relocatable library search path: {item}")
            versions = run("readelf", "--version-info", str(binary))
            glibc = sorted(set(re.findall(r"\bGLIBC_(\d+(?:\.\d+)+)", versions)), key=lambda s: tuple(map(int, s.split("."))))
            extra = {"glibcMinimum": glibc[-1] if glibc else None}
        else:
            raw = run("otool", "-L", str(binary))
            libraries = [line.strip().split(" (", 1)[0] for line in raw.splitlines()[1:]]
            for item in libraries:
                system = item.startswith(("/usr/lib/", "/System/Library/"))
                local = item.startswith(("@rpath/", "@loader_path/", "@executable_path/")) and item.rsplit("/", 1)[-1] in bundled
                if not system and not local:
                    raise ValueError(f"unbundled/non-relocatable dependency: {binary.name}: {item}")
            load = run("otool", "-l", str(binary))
            search = re.findall(r"cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset", load)
            if any(not p.startswith(("@loader_path", "@executable_path")) for p in search):
                raise ValueError(f"non-relocatable macOS rpath: {search}")
            extra = {"macosMinimum": re.findall(r"\bminos ([\d.]+)", load)}
        result.append({"path": binary.relative_to(root).as_posix(), "libraries": libraries,
                       "searchPaths": search, **extra})
    return {"schema": 1, "target": target, "nativeFiles": result}
