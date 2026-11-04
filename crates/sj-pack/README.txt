sj-pack: packs maps for SourceJump's replay viewer and zone editor

Windows: put sj-pack.exe in the folder with your .bsp or .bsp.bz2 files and
double-click it. The first time, Windows may say "Windows protected your PC";
click "More info", then "Run anyway".

Linux: run it from a terminal, in the folder with your maps or naming the
folder. Don't double-click it: it then runs with no window, so you can't see
its progress or stop it.
    chmod +x sj-pack
    ./sj-pack /path/to/maps

It packs each map into a "packed" folder next to them, several maps at once.
Large maps take a few minutes each. You can stop it any time (close the window
or press Ctrl+C) and run it again later: maps already packed are skipped.

When it's done, upload the "packed" folder on SourceJump's staff Maps page.

Options: sj-pack [folder] [--jobs N]
    --jobs N   maps packed at once (default: up to 4). Use fewer if the
               computer runs out of memory on very large maps.
