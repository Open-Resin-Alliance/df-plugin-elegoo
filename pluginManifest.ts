import elegooMarsPrinters from './printers/mars-series.json';
import elegooSaturnPrinters from './printers/saturn-series.json';
import elegooJupiterPrinters from './printers/jupiter-series.json';

function resolvePresetImagePath(imageAssetPath: unknown): string | undefined {
  if (typeof imageAssetPath !== 'string' || !imageAssetPath.trim()) return undefined;
  const trimmed = imageAssetPath.trim();
  if (trimmed.startsWith('/') || trimmed.startsWith('http')) return trimmed;
  const normalized = trimmed.startsWith('./') ? trimmed.slice(2) : trimmed;
  return `/plugins/elegoo/printers/${normalized}`;
}

function mapPresets(presets: any[]) {
  return presets.map((preset) => ({
    ...preset,
    imageAssetPath: resolvePresetImagePath(preset.imageAssetPath),
  }));
}

export const ELEGOO_PLUGIN_MANIFEST = {
  schemaVersion: 1,
  id: 'elegoo-builtin',
  name: 'ELEGOO Plugin',
  version: '0.3.0',
  description: 'ELEGOO printer profiles and Goo format support for DragonFruit.',
  author: 'Open Resin Alliance',
  homepage: 'https://github.com/Open-Resin-Alliance/df-plugin-elegoo',
  printerPresets: [
    ...mapPresets(elegooMarsPrinters as any[]),
    ...mapPresets(elegooSaturnPrinters as any[]),
    ...mapPresets(elegooJupiterPrinters as any[]),
  ],
  materialTemplates: [],
};
