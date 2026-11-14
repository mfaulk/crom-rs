#!/usr/bin/env bash
# Sandbox feasibility spike. Runs the same probes under each available backend.
# env: SPIKE_DIR (holds tinycrate/, depcrate/, sbx binary at $SBX), BACKENDS,
#      IMG_CARGO (container image with cc), IMG_TRUE (small image), N (timing reps)
set -u
SPIKE_DIR=${SPIKE_DIR:?}
SBX=${SBX:-$SPIKE_DIR/sbx/target/release/sbx}
BACKENDS=${BACKENDS:-"bare landlock bwrap podman"}
IMG_CARGO=${IMG_CARGO:-docker.io/library/rust:1-slim}
IMG_TRUE=${IMG_TRUE:-docker.io/library/alpine:3}
N=${N:-20}
TC=$(dirname "$(dirname "$(rustup which cargo)")")
CH=${CARGO_HOME:-$HOME/.cargo}

ROOT=$(mktemp -d "${TMPDIR:-/tmp}/spike.XXXXXX")
WS=$ROOT/ws; OUT=$ROOT/outside; FAKEHOME=$ROOT/home
mkdir -p "$WS" "$OUT" "$FAKEHOME/.ssh"
echo "SYNTHETIC-SECRET" > "$FAKEHOME/.ssh/id_fake"
cp -r "$SPIKE_DIR/tinycrate" "$SPIKE_DIR/depcrate" "$WS/"
( cd "$SPIKE_DIR/depcrate" && cargo fetch -q )
export FAKE_PROVIDER_KEY=sk-fake-should-not-leak

python3 -u -m http.server 0 --bind 127.0.0.1 >"$ROOT/http.log" 2>&1 &
HTTP_PID=$!
sleep 1
PORT=$(grep -o 'port [0-9]*' "$ROOT/http.log" | awk '{print $2}')

cat > "$WS/probe.sh" <<EOF
r() { n=\$1; shift; if "\$@" >/dev/null 2>&1; then echo "\$n=ALLOWED"; else echo "\$n=blocked"; fi; }
r write_outside sh -c 'echo x > $OUT/pwned'
r read_secret cat $FAKEHOME/.ssh/id_fake
r net_tcp_local bash -c 'exec 3<>/dev/tcp/127.0.0.1/$PORT'
r net_udp_connect bash -c 'exec 3<>/dev/udp/192.0.2.1/53'
r net_tcp_public timeout 5 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443'
if command -v python3 >/dev/null; then python3 -c '
import os,resource,time
resource.setrlimit(resource.RLIMIT_NPROC,(256,256)); n=0
for i in range(300):
    try:
        pid=os.fork()
    except OSError:
        break
    if pid==0:
        time.sleep(3); os._exit(0)
    n+=1
print("nproc256_forks=%d" % n)'; else echo nproc256_forks=n/a; fi
if [ -n "\${FAKE_PROVIDER_KEY:-}" ]; then echo env_key=LEAKED; else echo env_key=absent; fi
setsid sleep 4242 </dev/null >/dev/null 2>&1 &
echo spawned_setsid=yes
EOF

ENVS="PATH=$TC/bin:/usr/local/bin:/usr/bin:/bin HOME=$WS CARGO_HOME=$CH TMPDIR=$WS/.tmp CARGO_TARGET_DIR=$WS/target"
mkdir -p "$WS/.tmp"

run() { # backend, cmd...
  local b=$1; shift
  case $b in
    bare) env $ENVS FAKE_PROVIDER_KEY=$FAKE_PROVIDER_KEY "$@" ;;
    landlock)
      env $ENVS FAKE_PROVIDER_KEY=$FAKE_PROVIDER_KEY "$SBX" \
        --rw "$WS" --rw /dev/null --ro /usr --ro /etc --ro /bin --ro /lib --ro /lib64 \
        --ro /proc --ro /dev/urandom --ro "$TC" --ro "$CH" \
        --env PATH --env HOME --env CARGO_HOME --env TMPDIR --env CARGO_TARGET_DIR -- "$@" ;;
    landlock-scope)
      systemd-run --user --scope -q -p TasksMax=512 -p MemoryMax=2G \
        env $ENVS FAKE_PROVIDER_KEY=$FAKE_PROVIDER_KEY "$SBX" \
        --rw "$WS" --rw /dev/null --ro /usr --ro /etc --ro /bin --ro /lib --ro /lib64 \
        --ro /proc --ro /dev/urandom --ro "$TC" --ro "$CH" \
        --env PATH --env HOME --env CARGO_HOME --env TMPDIR --env CARGO_TARGET_DIR -- "$@" ;;
    bwrap)
      bwrap --unshare-all --die-with-parent --new-session --tmpfs /tmp \
        --ro-bind /usr /usr --ro-bind /etc /etc \
        --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 \
        --ro-bind "$TC" "$TC" --ro-bind "$CH" "$CH" --bind "$WS" "$WS" \
        --proc /proc --dev /dev --clearenv \
        $(for e in $ENVS; do printf -- '--setenv %s %s ' "${e%%=*}" "${e#*=}"; done) \
        --chdir "$WS" "$@" ;;
    podman|docker)
      local img=$IMG_CARGO; [ "$1" = true ] && img=$IMG_TRUE
      local extra=(--user "$(id -u):$(id -g)"); [ $b = podman ] && extra=(--userns=keep-id --security-opt label=disable)
      $b run --rm --network none --pids-limit 512 --memory 2g "${extra[@]}" \
        -v "$WS:$WS" -v "$TC:$TC:ro" -v "$CH:$CH:ro" -w "$WS" \
        $(for e in $ENVS; do printf -- '-e %s ' "$e"; done) \
        "$img" "$@" ;;
  esac
}

ms() { echo $(( ($2 - $1) / 1000000 )); }

echo "host: kernel=$(uname -r) user_procs=$(ps -u "$(id -u)" --no-headers | wc -l) ulimit_u=$(ulimit -u)"
for b in $BACKENDS; do
  echo "=== backend: $b"
  case $b in
    bare) ;;
    landlock) [ -x "$SBX" ] || { echo "  unavailable"; continue; } ;;
    landlock-scope) systemd-run --user --scope -q true >/dev/null 2>&1 || { echo "  unavailable (systemd-run --user --scope failed)"; continue; } ;;
    *) command -v $b >/dev/null 2>&1 || { echo "  unavailable"; continue; } ;;
  esac
  if [ $b = podman ] || [ $b = docker ]; then
    $b pull -q "$IMG_TRUE" >/dev/null 2>&1; $b pull -q "$IMG_CARGO" >/dev/null 2>&1
  fi
  rm -f "$OUT/pwned"
  ( cd "$WS" && run $b bash "$WS/probe.sh"; echo "probe_rc=$?" ) 2>&1 | sed 's/^/  /'
  sleep 0.5
  if pgrep -f '^sleep 4242$' >/dev/null; then echo "  setsid_child=SURVIVED"; pkill -f '^sleep 4242$'; else echo "  setsid_child=reaped"; fi
  [ -e "$OUT/pwned" ] && echo "  (outside file exists)"
  # first true includes cold-start; then N warm reps
  run $b true >/dev/null 2>&1
  t0=$(date +%s%N); for _ in $(seq "$N"); do run $b true >/dev/null 2>&1; done; t1=$(date +%s%N)
  echo "  true_x${N}_mean_ms=$(( $(ms $t0 $t1) / N ))"
  for c in tinycrate depcrate; do
    cmd=(sh -c "cd $WS/$c && cargo test --offline -q 2>&1 | grep -E 'test result|^warning|^error' | head -2")
    rm -rf "$WS/target"
    t0=$(date +%s%N); outp=$(run $b "${cmd[@]}" 2>&1); rc=$?; t1=$(date +%s%N)
    echo "  cargo_test_${c}: rc=$rc ms=$(ms $t0 $t1) last=[$(echo "$outp" | tr '\n' ' ' | cut -c1-160)]"
  done
done
kill $HTTP_PID 2>/dev/null
rm -rf "$ROOT" 2>/dev/null
exit 0
