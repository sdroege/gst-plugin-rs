#!/usr/bin/env python3
#
# check-readme-against-plugins-list.py:
#
# Check that the README.md covers all plugins in the plugin list.

import json
import sys

# Read README.md
with open('README.md', encoding='utf-8') as f:
    readme = f.read()

# Read plugins doc cache
with open('docs/plugins/gst_plugins_cache.json', 'r', encoding='utf-8') as f:
    docs = json.load(f)

# === utility functions ===


def entry_exists(bullet_name):
    bullet_line = f'- `{bullet_name}`:'
    return bullet_line in readme

# === Plugins ===


plugins = list(docs.keys())

missing_plugins = []
missing_factories = []

# Some plugins are listed with their subdir which doesn't match the plugin name
plugins_remap = {
    'rsflv': 'flavors',
    'textahead': 'ahead',
    'textwrap': 'wrap',
    'textaccumulate': 'accumulate',
}

factories_remap = {}

for plugin_name in plugins:

    plugin = docs[plugin_name]

    name = plugins_remap.get(plugin_name, plugin_name)

    exists = entry_exists(name)

    if not exists and name.startswith('rs'):
        name = name[2:]
        exists = entry_exists(name)

    print('===================================')
    print()

    if not exists:
        print(f'Plugin {name}: Missing!')
        missing_plugins += [name]
    else:
        print(f'Plugin {name}: Ok')

    print()

    plugin_name = name

    factories = list(plugin['elements'].keys())

    for factory_name in factories:
        name = factories_remap.get(factory_name, factory_name)

        exists = entry_exists(name)

        if not exists:
            print(f' - {name}: Missing!')
            missing_factories += [f'{plugin_name}/{name}']
        else:
            print(f' - {name}: Ok')

    print()

print('===================================')
print()

if missing_plugins:
    print('Missing plugins in README.md:\n - {}\n'.format(
        '\n - '.join(missing_plugins)))

if missing_factories:
    print('Missing elements or plugin factories in README.md:\n - {}\n'.format(
        '\n - '.join(missing_factories)))

sys.exit(1 if missing_plugins or missing_factories else 0)
