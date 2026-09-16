Remove the `./hooks` package export, which pointed at a module that no longer exists.

`core/hooks` was folded into the unified extensions system long ago, but the export map still
advertised `./hooks`, so importing `@earendil-works/pi-coding-agent/hooks` failed to resolve.
