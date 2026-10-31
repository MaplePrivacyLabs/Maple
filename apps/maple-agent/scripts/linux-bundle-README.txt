Maple Work for Linux
====================

Install once, then launch from your desktop's application menu.

Requirements: x86_64 Linux with glibc 2.31 or newer (Ubuntu 20.04, Debian 11,
Fedora 32 or later), a graphics driver with Vulkan or OpenGL support, and the
standard desktop libraries libxkbcommon, libxkbcommon-x11, libX11, libXi,
libxcb and ALSA (libasound). Desktop installations include these. Maple Work
uses your system's own libraries and GPU drivers, like other desktop apps.

1. Extract the entire .tar.gz archive. Do not open AppRun inside Archive Manager
   (File Roller): files inside the archive cannot be launched there.
2. Open a terminal in the extracted application folder (the folder containing
   this README.txt and install.sh), then run:

       ./install.sh

   Run as your normal desktop user, without sudo. Python 3 is required for
   installation. If extracting in a terminal, use tar -xzpf ARCHIVE.tar.gz to
   preserve the required file permissions.
3. Open Activities / Show Applications and search for Maple Work (or Maple Work
   Dev for a Dev bundle). Pin that application to your dock if desired.

The installer copies the application into ~/.local and creates its application
menu entry. Keep the extracted folder intact until installation completes; after
that, the installed copy does not depend on the download folder.

AppRun is a terminal entrypoint, not a double-click launcher. GNOME Files may
open shell scripts instead of executing them; this does not mean Maple Work is
unsupported. Do not change your system's file associations or script-execution
security settings. To run without installing, use ./AppRun from a terminal in
this extracted folder.

Already installed? Launch Maple Work from the application menu and use its
built-in updater. The first-install helper refuses to overwrite an existing
installation and leaves account, settings and task data untouched.
