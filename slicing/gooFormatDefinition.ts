import type { SlicingFormatDefinition } from '@/features/slicing/formats/types';

export const GOO_FORMAT_DEFINITION: SlicingFormatDefinition = {
  id: 'goo.v1',
  outputFormat: '.goo',
  displayName: 'GOO',
  ownership: 'plugin',
  layerDataKind: 'raw-mask',
  pluginId: 'elegoo',
  formatVersions: [
    { value: 'v1.2', label: 'GOO V1.2', isDefault: true },
    { value: 'v5.1', label: 'GOO V5.1 (Jupiter 2)' }
  ],
  settingsModes: [
    { value: 'simple', label: 'Simple', isDefault: true },
    { value: 'twostage', label: 'Two Stage' },
    { value: 'tilting', label: 'Tilting Vat' }
  ],
  rustModulePath: 'formats::goo',
  wasmExportName: 'encode_goo_container',
  notes: 'Goo binary encoder using big-endian raw mask layers.',
};
