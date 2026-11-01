//go:build librclone_full

// Every rclone backend (about 3x the size): LIBRCLONE_FULL=1 scripts/build-librclone.sh
package main

import _ "github.com/rclone/rclone/backend/all"
