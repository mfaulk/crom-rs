#!/usr/bin/env bash
# M8 plugin demo: validate, install, list, show, disable, enable, reload,
# uninstall. No network access required.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEMO_DIR="${REPO_ROOT}/examples/plugin-demo"
PLUGIN_SOURCE="${DEMO_DIR}/plugins/rust-quality"
TARGET="${DEMO_DIR}/target-project"
WORKDIR="$(mktemp -d)"

echo "[demo] using workdir ${WORKDIR}"
cp -R "${TARGET}/." "${WORKDIR}/"
mkdir -p "${WORKDIR}/.agent"

cd "${WORKDIR}"

CARGO_BIN="${REPO_ROOT}/target/debug/crom"
if [ ! -x "${CARGO_BIN}" ]; then
  (cd "${REPO_ROOT}" && cargo build --bin crom)
fi

echo "[demo] validate"
"${CARGO_BIN}" plugin validate "${PLUGIN_SOURCE}"

echo "[demo] install (local scope, --enable)"
"${CARGO_BIN}" plugin install "${PLUGIN_SOURCE}" --scope local --enable --cwd "${WORKDIR}"

echo "[demo] list"
"${CARGO_BIN}" plugin list --cwd "${WORKDIR}"

echo "[demo] show"
"${CARGO_BIN}" plugin show rust-quality --cwd "${WORKDIR}"

echo "[demo] disable"
"${CARGO_BIN}" plugin disable rust-quality --scope local --cwd "${WORKDIR}"

echo "[demo] enable"
"${CARGO_BIN}" plugin enable rust-quality --scope local --cwd "${WORKDIR}"

echo "[demo] reload"
"${CARGO_BIN}" plugin reload --cwd "${WORKDIR}"

echo "[demo] uninstall (--keep-data --yes)"
"${CARGO_BIN}" plugin uninstall rust-quality --scope local --keep-data --yes --cwd "${WORKDIR}"

echo "[demo] OK"
