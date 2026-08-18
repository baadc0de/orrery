#!/usr/bin/env bash
# Emit one line per completed sweep point, then exit when the driver does.
prev=0
while true; do
  n=$(ls /home/baadc0de/fenced-sweep/ | grep -c -v driver || true)
  if [ "$n" != "$prev" ]; then echo "sweep points complete: $n/40"; prev=$n; fi
  if ! pgrep -f fenced-sweep-driver >/dev/null; then echo "sweep driver exited (points=$n)"; break; fi
  sleep 20
done
