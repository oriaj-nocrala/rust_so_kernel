# Phase 5.1 of docs/gpu/gpu-plan.md, on the Ryzen:
#   scripts/metal-run.sh --kconf 'gpu=dispstate' scripts/metal-jobs/gpu-dispstate.sh
# Prints /proc/dispstate into the log (reads only: nothing is written to
# the display) and fails unless the GOP's head 0 is the 1080p60 mode the
# trace saw before nouveau's first update (nvgpu/fixtures/
# modeset-core-round1.txt, ARMED column) and window 0's surface is the GOP
# framebuffer. The full per-method comparison with that column is done on
# the host from the log (`core MMMM VVVVVVVV` lines).
cat /proc/dispstate
fail=0
need() { grep -q "$2" "$1" || { echo "gpu-dispstate: MISSING in $1: $2"; fail=1; }; }
need /proc/dispstate '^head0: raster 2200x1125 viewport 1920x1080 pixel clock 148500000 Hz$'
need /proc/dispstate '^sor1: owner heads 0x01 protocol 0x9$'
need /proc/dispstate '^window owners: 0 '
need /proc/dispstate '^window0: 1920x1080 '
need /proc/dispstate 'window0 offset matches it, pitch matches'
need /proc/dispstate '^core 0320 00000901$'
need /proc/dispstate '^core 1010 00110780$'
exit $fail
