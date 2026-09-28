units := env("HOME") / ".config/systemd/user"

# Build, install the binary to ~/.local/bin and enable the auto-update timer.
install:
    cargo build --release
    install -Dm755 target/release/clash-man ~/.local/bin/clash-man
    install -Dm644 -t {{units}} systemd/clash-man-update.service systemd/clash-man-update.timer
    systemctl --user daemon-reload
    systemctl --user enable --now clash-man-update.timer

uninstall:
    -systemctl --user disable --now clash-man-update.timer
    rm -f {{units}}/clash-man-update.service {{units}}/clash-man-update.timer ~/.local/bin/clash-man
    systemctl --user daemon-reload
