#!/usr/bin/env python3
"""Convert git-sourced deps to crates.io versions in root Cargo.toml.

Replaces git-sourced deps with their crates.io equivalents, preserving
version, features, package, optional, default-features, and other keys.
"""

import sys
import re
import os
import tomllib

GIT_KEYS = {'git', 'branch', 'rev', 'tag'}


def dep_value_str(spec):
    kept = {k: v for k, v in spec.items() if k not in GIT_KEYS}
    if not kept:
        return ''
    if list(kept.keys()) == ['version']:
        return f'"{kept["version"]}"'
    parts = []
    for k, v in kept.items():
        if isinstance(v, str):
            parts.append(f'{k} = "{v}"')
        elif isinstance(v, bool):
            parts.append(f'{k} = {"true" if v else "false"}')
        elif isinstance(v, list):
            items = ', '.join(f'"{x}"' if isinstance(x, str) else str(x) for x in v)
            parts.append(f'{k} = [{items}]')
        elif isinstance(v, int):
            parts.append(f'{k} = {v}')
        else:
            parts.append(f'{k} = {v}')
    return '{ ' + ', '.join(parts) + ' }'


def main():
    root = sys.argv[1] if len(sys.argv) > 1 else '.'
    path = os.path.join(root, 'Cargo.toml')

    with open(path, 'rb') as f:
        data = tomllib.load(f)
    ws_deps = data.get('workspace', {}).get('dependencies', {})

    replacements = {}
    for name, spec in ws_deps.items():
        if isinstance(spec, dict) and 'git' in spec:
            val = dep_value_str(spec)
            if val:
                replacements[name] = val

    if not replacements:
        print("No git deps found")
        return False

    with open(path) as f:
        lines = f.readlines()

    new_lines = []
    changed = False
    in_deps = False
    dep_re = re.compile(r'^(\s*)([\w-]+)\s*=\s*')

    for line in lines:
        stripped = line.strip()
        if stripped == '[workspace.dependencies]':
            in_deps = True
            new_lines.append(line)
        elif in_deps and stripped.startswith('['):
            in_deps = False
            new_lines.append(line)
        elif in_deps and (m := dep_re.match(line)):
            name = m.group(2)
            if name in replacements:
                new_lines.append(f'{m.group(1)}{name} = {replacements[name]}\n')
                changed = True
                continue
            new_lines.append(line)
        else:
            new_lines.append(line)

    if changed:
        with open(path, 'w') as f:
            f.writelines(new_lines)
        print(f"Converted {len(replacements)} git deps: {', '.join(sorted(replacements))}")
    else:
        print("No changes needed")
    return changed


if __name__ == '__main__':
    main()
