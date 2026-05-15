ePHPm — Embedded PHP Manager
============================

All-in-one PHP application server in a single binary. Embeds PHP via FFI,
includes an HTTP server, MySQL connection pool, embedded SQLite, KV store,
clustering, and TLS / ACME.

This archive contains:
  ephpm        the binary (use `ephpm.exe` on Windows)
  LICENSE      MIT
  README.txt   this file

Quick start
-----------

Linux / macOS:

    sudo ./ephpm install

Installs the binary to /usr/local/bin/ephpm, drops a default config in
/etc/ephpm/ephpm.toml, registers a systemd unit (Linux) or launchd plist
(macOS), and starts the service. The HTTP listener comes up on port 8080.

Windows (Administrator PowerShell):

    .\ephpm.exe install

Installs into "C:\Program Files\ephpm\", adds itself to PATH, registers
a Windows service, and starts.

Docker
------

If you'd rather not install anything:

    docker run -p 8080:8080 ephpm/ephpm:latest

Tag scheme:
  ephpm/ephpm:<release>-php<phpver>   pinned ePHPm release × pinned PHP patch
  ephpm/ephpm:<release>-php<minor>    pinned ePHPm release × rolling PHP minor
  ephpm/ephpm:<minor>                 rolling latest release × rolling PHP minor
  ephpm/ephpm:latest                  rolling latest release, default PHP minor

Manage the service
------------------

The same subcommands work on every platform — they wrap systemd /
launchd / the Windows service controller:

    sudo ephpm start       # start the service
    sudo ephpm stop        # stop the service
    sudo ephpm restart     # restart
    sudo ephpm status      # show status
    sudo ephpm uninstall   # remove the service + binary

Documentation
-------------

  Homepage:        https://ephpm.dev
  Source:          https://github.com/ephpm/ephpm
  Releases:        https://github.com/ephpm/ephpm/releases
  Issue tracker:   https://github.com/ephpm/ephpm/issues
