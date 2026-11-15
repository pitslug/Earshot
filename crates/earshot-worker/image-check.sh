#!/bin/sh
# The summary worker image's check (T11.6), run on a GitHub Actions runner by linux-check and
# relay-image before anything is pushed: `sh crates/earshot-worker/image-check.sh <image>`.
#
# 1. The slim image's libraries, as ldd reads them (fails on a missing one).
# 2. A worker that is not joined waits to be joined (T11.6d): it says so once across several
#    looks, its health check reports `not_joined`, and `docker stop` ends the wait with exit
#    code 0. Then, in a second unjoined container, the files `join` writes (marker, invented key
#    and token) keep it waiting until the marker goes, after which it runs.
# 3. A joined worker (invented vault files made here for this run, owner-only, against a relay
#    address that does not exist) runs, retries the relay and reaches Docker's `healthy`.
# Prints image output and fixed words only; the vault files are deleted at the end.
set -eu
image="$1"
uid=10001

echo "== libraries"
docker run --rm --entrypoint ldd "${image}" /usr/local/bin/earshot-worker | tee ldd.txt
if grep -q "not found" ldd.txt; then echo "a shared library is missing"; exit 1; fi
rm -f ldd.txt

docker volume create earshot-worker-unjoined > /dev/null
# Starts an unjoined worker named $1 on the unjoined volume and waits until its own health check,
# run as the documented bare command, says `not_joined`; leaves the answer in ${health}.
start_unjoined() {
  docker run -d --name "$1" -v earshot-worker-unjoined:/data \
    -e EARSHOT_WORKER_RELAY=https://relay.invalid \
    -e EARSHOT_WORKER_MODEL_SUBNET=172.31.250.0/24 \
    "${image}" run > /dev/null
  health=""
  for _ in $(seq 1 30); do
    health="$(docker exec "$1" earshot-worker health || true)"
    if [ "${health}" = "unhealthy: not_joined" ]; then break; fi
    if [ "$(docker inspect -f '{{.State.Running}}' "$1")" != "true" ]; then break; fi
    sleep 1
  done
  echo "health: ${health}"
}

echo "== not joined: waits, then stops cleanly"
start_unjoined earshot-worker-unjoined
# Past JOIN_POLL (5 s), so "waiting to join" once is checked across more than one look.
sleep 7
waiting="$(docker inspect -f '{{.State.Running}}' earshot-worker-unjoined)"
docker stop -t 20 earshot-worker-unjoined > /dev/null
code="$(docker inspect -f '{{.State.ExitCode}}' earshot-worker-unjoined)"
docker logs earshot-worker-unjoined > unjoined.txt 2>&1
cat unjoined.txt
echo "still running while waiting: ${waiting}; exit code after stop: ${code}"
docker rm earshot-worker-unjoined > /dev/null
test "${health}" = "unhealthy: not_joined"
test "${waiting}" = "true"
test "${code}" = "0"
test "$(grep -cx "waiting to join" unjoined.txt)" = "1"
grep -qx "stopped" unjoined.txt
if grep -qx "running" unjoined.txt; then echo "a worker that never joined ran"; exit 1; fi
rm -f unjoined.txt

echo "== not joined: joined while it waits"
start_unjoined earshot-worker-joining
sleep 7
# What `join` leaves in the volume, written as the container's user with invented values (no
# relay exists here to join): the join marker first, written to a temporary name and renamed as
# `join` does so a look never reads it half written, then the key and the token.
docker exec earshot-worker-joining sh -c \
  'umask 077 && mkdir -p /data/vault && echo "joining $(date +%s)" > /data/vault/joining.tmp && mv /data/vault/joining.tmp /data/vault/joining'
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' \
  | docker exec -i earshot-worker-joining sh -c 'umask 077 && cat > /data/vault/key'
echo "an-invented-token-for-the-image-check" \
  | docker exec -i earshot-worker-joining sh -c 'umask 077 && cat > /data/vault/token'
# Past a look with the marker still there: the run must keep waiting until the join ends.
sleep 7
held="$(docker logs earshot-worker-joining 2>&1 | grep -cx "running" || true)"
docker exec earshot-worker-joining rm /data/vault/joining
ran=no
for _ in $(seq 1 15); do
  if docker logs earshot-worker-joining 2>&1 | grep -qx "running"; then ran=yes; break; fi
  sleep 1
done
docker stop -t 20 earshot-worker-joining > /dev/null
code="$(docker inspect -f '{{.State.ExitCode}}' earshot-worker-joining)"
docker logs earshot-worker-joining > joining.txt 2>&1
cat joining.txt
echo "ran while the marker was there: ${held}; ran after: ${ran}; exit code after stop: ${code}"
docker rm earshot-worker-joining > /dev/null
docker volume rm earshot-worker-unjoined > /dev/null
test "${health}" = "unhealthy: not_joined"
test "${held}" = "0"
test "${ran}" = "yes"
test "${code}" = "0"
test "$(grep -cx "waiting to join" joining.txt)" = "1"
rm -f joining.txt

echo "== joined"
mkdir -m 0700 smoke
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > smoke/key
echo "an-invented-token-for-the-image-check" > smoke/token
chmod 0600 smoke/key smoke/token
sudo chown "${uid}:${uid}" smoke smoke/key smoke/token
docker volume create earshot-worker-check > /dev/null
docker run -d --name earshot-worker-check \
  --health-interval 2s --health-start-period 5s --health-retries 3 \
  -v earshot-worker-check:/data \
  -v "${PWD}/smoke:/run/secrets:ro" \
  -e EARSHOT_WORKER_RELAY=https://relay.invalid \
  -e EARSHOT_WORKER_MODEL_SUBNET=172.31.250.0/24 \
  -e EARSHOT_WORKER_KEY_FILE=/run/secrets/key \
  -e EARSHOT_WORKER_TOKEN_FILE=/run/secrets/token \
  "${image}" > /dev/null
status=starting
for _ in $(seq 1 60); do
  status="$(docker inspect -f '{{.State.Health.Status}}' earshot-worker-check)"
  if [ "${status}" = "healthy" ] || [ "${status}" = "unhealthy" ]; then break; fi
  if [ "$(docker inspect -f '{{.State.Running}}' earshot-worker-check)" != "true" ]; then break; fi
  sleep 2
done
echo "health: ${status}"
docker exec earshot-worker-check /usr/local/bin/earshot-worker health || true
docker logs earshot-worker-check
docker stop -t 20 earshot-worker-check > /dev/null
echo "exit code after stop: $(docker inspect -f '{{.State.ExitCode}}' earshot-worker-check)"
docker rm earshot-worker-check > /dev/null
docker volume rm earshot-worker-check > /dev/null
sudo rm -rf smoke
test "${status}" = "healthy"
echo "== image check passed"
