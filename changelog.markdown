## Changes

Releases are named using the date, time and git commit
hash.

### Nightly

A bleeding edge build is produced continually (at least
daily) from the master branch.  It may not be usable and
the feature set may change.  As features stabilize some
brief notes about them may accumulate here.

* No new changes yet!

### 20200113-214446-bb6251f

* Added `color_scheme` configuration option and more than 200 color schemes
* Improved resize behavior; lines that were split due to
  the width of the terminal are now rewrapped on resize.
  [Issue 14](https://github.com/wez/wezterm/issues/14)
* Double-click and triple-click and hold followed by a drag now extends
  the selection by word and line respectively.
* The OSC 7 (CurrentWorkingDirectory) escape sequence is now supported; wezterm records the cwd in a tab and that will be used to set the working directory when spawning new tabs in the same domain.  You will need to configure your shell to emit OSC 7 when appropriate.
* [Changed Backspace/Delete handling](https://github.com/wez/wezterm/commit/f0e94084d1df36009b879b06e9cfd2be946168e8)
* Added `MoveTabRelative` for changing the ordering of tabs within a window
  using key assignments `CTRL+SHIFT+PageUp` and `CTRL+SHIFT+PageDown`
* [The multiplexer protocol is undergoing major changes](https://github.com/wez/wezterm/issues/106).
  The multiplexer will now raise an error if the client and server are incompatible.
* Fixed an issue where wezterm would linger for a few seconds after the last tab was closed
* Fixed an issue where wezterm wouldn't repaint the screen after a tab was closed
* Clicking the OS window close button in the titlebar now closes the window rather than the active tab
* Added `use_ime` option to optionally disable the use of the IME on macOS.  You might consider enabling this if you don't like the way that the IME swallows key repeats for some keys.
* Fix an [issue](https://github.com/knsd/daemonize/pull/39) where the pidfile would leak into child processes and block restarting the mux server
* Fix an issue where the title bars of remote tabs were not picked up at domain attach time
* Fixed selection and scrollbar position for multiplexer tabs
* Added `ScrollByPage` key assignment and moved the `SHIFT+PageUp` handling up to the
  gui layer so that it can be rebound.
* X11: a single mouse wheel tick now scrolls by 5 rows rather than 1
* Wayland: normalize line endings to unix line endings when pasting
* Windows: fixed handling of focus related messages, which impacted both the appearance of
  the text cursor and copy and paste handling.
* When hovering over implicitly hyperlinked items, we no longer show the underline for every other URL with the same destination

### 20191229-193639-e7aa2f3

* Fixed a hang when using middle mouse button to paste
* Recognize 8-bit C1 codes encoded as UTF-8, which are used in the Fedora 31 bash prexec notification for gnome terminal
* Ensure that underlines are a minimum of 1 pixel tall
* Reduced CPU utilization on some Wayland compositors
* Added `$WEZTERM_CONFIG_FILE` to the start of the config file search path
* Added new font rendering options:

```
font_antialias = "Subpixel" # None, Greyscale, Subpixel
font_hinting = "Full" # None, Vertical, VerticalSubpixel, Full
```

* Early startup errors now generate a "toast" notification, giving you more of a clue about what went wrong
* We now use the default configuration if the config file had errors, rather than refusing to start
* Wayland compositors: Improved detection of display scaling on startup
* Added `harfuzz_features` option to specify stylistic sets for fonts such as Fira Code, and to control various typographical options
* Added a `window_padding` config section to add padding to the window display
* We now respect [DECSCUSR and DECTCEM](https://github.com/wez/wezterm/issues/7) escape sequence to select between hidden, block, underline and bar cursor types, as well as blinking cursors.  New configuration options have been added to control the appearance and blink rate.
* We now support an optional basic scroll bar.  The scroll bar occupies the right window padding and has a configurable color.  Scroll bars are not yet supported for multiplexer connections and remain disabled by default for the moment.
* Color scheme changes made in the config file now take effect at config reload time for all tabs that have not applied a dynamic color scheme.

### 20191218-101156-bf35707

* Configuration errors detected during config loading are now shown as a system notification
* New `font_dirs` configuration option to specify a set of dirs to search for fonts. Useful for self-contained wezterm deployments.
* The `font_system` option has been split into `font_locator`, `font_shaper` and `font_rasterizer` options.
* Don't allow child processes to inherit open font files on posix systems!
* Disable Nagle's algorithm for `wezterm ssh` sessions
* Add native Wayland window system support

### 20191124-233250-cb9fd7d

* New tab bar UI displays tabs and allows creating new tabs
* Configuration file changes are hot reloaded and take effect automatically on save
* `wezterm ssh user@host` for ad-hoc SSH sessions. You may also define SSH multiplexer sessions.
* `wezterm serial /dev/ttyUSB0` to connect to your Arduino
* `wezterm imgcat /some/image.png` to display images inline in the terminal using the iTerm2 image protocol
* IME support on macOS and Windows systems
* Automatic fallback to software rendering if no GPU is available (eg: certain types of remote desktop sessions)


