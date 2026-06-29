Drop audio files here (.mp3 .ogg .wav .flac .m4a .aac) and they appear in the
WINAMP player on the page.

Discovery order:
  1. tracks/manifest.json  — a JSON array of filenames, e.g. ["a.mp3","b.ogg"]
  2. otherwise the player parses the web server's directory listing of tracks/
     (works out of the box with `python3 -m http.server`).
