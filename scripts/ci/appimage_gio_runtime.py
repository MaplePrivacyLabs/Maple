#!/usr/bin/env python3
"""Check that an AppImage's own GIO and GTK can do TLS and open a file chooser.

Run with LD_LIBRARY_PATH and the GIO variables pointing into the AppDir. The
library lookups use the bundled copies, so a working host cannot hide a gap.
"""

import ctypes
import sys
from pathlib import Path

FILE_CHOOSER_SCHEMA = b"org.gtk.Settings.FileChooser"
FILE_CHOOSER_TEMPLATE = b"/org/gtk/libgtk/ui/gtkfilechooserwidget.ui"


def load(soname, appdir):
    library = ctypes.CDLL(soname, mode=ctypes.RTLD_GLOBAL)
    # dlopen reports only the name it was given, so ask the loader for the file.
    for line in Path("/proc/self/maps").read_text().splitlines():
        if soname in line:
            path = Path(line.split()[-1])
            if appdir not in path.parents:
                raise OSError(f"{soname} loaded from outside the AppDir: {path}")
            return library
    raise OSError(f"{soname} is not mapped after loading")


def failures(appdir):
    gio = load("libgio-2.0.so.0", appdir)
    gtk = load("libgtk-3.so.0", appdir)

    gio.g_tls_backend_get_default.restype = ctypes.c_void_p
    gio.g_tls_backend_supports_tls.argtypes = [ctypes.c_void_p]
    backend = gio.g_tls_backend_get_default()
    if not (backend and gio.g_tls_backend_supports_tls(backend)):
        yield "GIO has no TLS backend, so every HTTPS request from the webview would fail"

    gio.g_settings_schema_source_get_default.restype = ctypes.c_void_p
    gio.g_settings_schema_source_lookup.restype = ctypes.c_void_p
    gio.g_settings_schema_source_lookup.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
    source = gio.g_settings_schema_source_get_default()
    if not (source and gio.g_settings_schema_source_lookup(source, FILE_CHOOSER_SCHEMA, True)):
        yield f"GSettings schema {FILE_CHOOSER_SCHEMA.decode()} is missing, so file dialogs would abort"

    # Fails without a display, but registers GTK's resources first.
    gtk.gtk_init_check.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    gtk.gtk_init_check(None, None)
    gio.g_resources_lookup_data.restype = ctypes.c_void_p
    gio.g_resources_lookup_data.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.c_void_p]
    if not gio.g_resources_lookup_data(FILE_CHOOSER_TEMPLATE, 0, None):
        yield f"GTK resource {FILE_CHOOSER_TEMPLATE.decode()} is missing, so file dialogs would abort"


def main():
    appdir = Path(sys.argv[1]).resolve()
    try:
        problems = list(failures(appdir))
    except OSError as error:
        problems = [str(error)]
    for problem in problems:
        print(f"AppImage runtime check failed: {problem}", file=sys.stderr)
    if problems:
        return 1
    print(f"verified-linux-appimage-gio-runtime  {appdir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
