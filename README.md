# df-plugin-elegoo

DragonFruit Plugin for Elegoo Printers

> **Asset source notice**
>
> Printer preview assets in this repository were sourced from the official Elegoo website.
> We are currently seeking explicit permission from Elegoo for their use and distribution.

## Mirroring

Mirror flags are reconciled against the supplied `Elegoo_officialPlatform.ini`
catalog. `MirrorX` and `MirrorY` are copied independently; models do not inherit
another model's mirroring within the same family. Existing dimensions, pixel
pitches, and raster resolutions are unchanged.

Mars 2 Pro and Saturn 2 14K have no matching entry in that reference, so their
previous mirror flags are retained, not treated as verified. Presets with
corrected mirrors increment `profileVersion` to offer saved official profiles
the update.

