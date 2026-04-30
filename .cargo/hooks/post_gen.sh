#!/bin/bash
# Replace template placeholders with the chosen project name.

set -euo pipefail

project_name="accordin"

sed -i "s/scx_bin/${project_name}/g" Cargo.toml
sed -i "s/scx_bin/${project_name}/g" gen-compile-commands.sh
sed -i "s/scx_bin/${project_name}/g" update-clangd.sh
