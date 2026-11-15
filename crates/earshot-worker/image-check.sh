#!/bin/sh
# The summary worker image's check (T11.6), run on a GitHub Actions runner by linux-check and
# relay-image before anything is pushed: `sh crates/earshot-worker/image-check.sh <image>`.
#
# 1. The slim image's libraries, as ldd reads them (fails on a missing one).
# 2. A worker that is not joined waits to be joined (T11.6d): it says so once, its health check
#    reports `not_joined`, and `docker stop` ends the wait with exit code 0.
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

echo "== not joined"
docker volume create earshot-worker-unjoined > /dev/null
docker run -d --name earshot-worker-unjoined -v earshot-worker-unjoined:/data \
  -e EARSHOT_WORKER_RELAY=https://relay.invalid \
  -e EARSHOT_WORKER_MODEL_SUBNET=172.31.250.0/24 \
  "${image}" run > /dev/null
health=""
for _ in $(seq 1 30); do
  health="$(docker exec earshot-worker-unjoined /usr/local/bin/earshot-worker health || true)"
  if [ "${health}" = "unhealthy: not_joined" ]; then break; fi
  if [ "$(docker inspect -f '{{.State.Running}}' earshot-worker-unjoined)" != "true" ]; then break; fi
  sleep 1
done
echo "health: ${health}"
waiting="$(docker inspect -f '{{.State.Running}}' earshot-worker-unjoined)"
docker stop -t 20 earshot-worker-unjoined > /dev/null
code="$(docker inspect -f '{{.State.ExitCode}}' earshot-worker-unjoined)"
docker logs earshot-worker-unjoined > unjoined.txt 2>&1
cat unjoined.txt
echo "still running while waiting: ${waiting}; exit code after stop: ${code}"
docker rm earshot-worker-unjoined > /dev/null
docker volume rm earshot-worker-unjoined > /dev/null
test "${health}" = "unhealthy: not_joined"
test "${waiting}" = "true"
test "${code}" = "0"
test "$(grep -cx "waiting to join" unjoined.txt)" = "1"
grep -qx "stopped" unjoined.txt
if grep -qx "running" unjoined.txt; then echo "a worker that never joined ran"; exit 1; fi
rm -f unjoined.txt

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
