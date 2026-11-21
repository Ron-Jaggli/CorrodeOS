#!/usr/bin/env bash
# Look for a window frame's lower rim through a fading window's body.
#
# A titled window's frame is its title bar and a little more, reaching
# under the top of the body. While the body is half transparent, that
# hidden edge would show through it as a line just under the title bar.
# This cuts a strip of the body's left margin from a screenshot (binary
# PPM) under the bar, where the terminal draws no text, and prints the
# largest step in brightness from one row to the next down it, and the
# row: smooth is a few levels, an edge more.
#
#   rim.sh SHOT.ppm LEFT TOP [BAR] [RADIUS]
#
# LEFT and TOP are the window's title bar's top left corner. Needs only
# netpbm and awk, which the build container has.
set -euo pipefail
shot=$1 left=$2 top=$3 bar=${4:-28} radius=${5:-12}
# Inside the body, clear of its left edge and of the text, which starts
# thirteen pixels in.
x=$((left + 6)) y=$((top + bar + 2)) columns=6 rows=$((radius + 8))
pamcut -left "$x" -top "$y" -width "$columns" -height "$rows" "$shot" \
  | pnmtoplainpnm \
  | awk -v first="$y" '
      # The plain header: magic, width, height, maxval; then the samples.
      { for (i = 1; i <= NF; i++) words[++n] = $i }
      END {
          width = words[2]; height = words[3]; at = 5
          for (row = 0; row < height; row++) {
              sum = 0
              for (sample = 0; sample < 3 * width; sample++) sum += words[at++]
              mean[row] = sum / (3 * width)
          }
          worst = 0; where = first
          for (row = 1; row < height; row++) {
              step = mean[row] - mean[row - 1]
              if (step < 0) step = -step
              if (step > worst) { worst = step; where = first + row }
          }
          printf "%.1f %d\n", worst, where
      }'
