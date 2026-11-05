# assets/ — inputs that are not software

Scenario files, recorded traces, sensor captures, a CARLA map — anything the experiment
reads that is not a loadable and not a manifest.

Large binaries do not belong in git. Keep the small ones here; for the rest, keep a
fetch script in `tools/` and the checksum here, so what ran is still identifiable.
