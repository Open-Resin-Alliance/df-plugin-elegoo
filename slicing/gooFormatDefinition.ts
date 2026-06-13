import type { SlicingFormatDefinition } from '@/features/slicing/formats/types';

export const GOO_FORMAT_DEFINITION: SlicingFormatDefinition = {
  id: 'goo.v1',
  outputFormat: '.goo',
  displayName: 'GOO',
  ownership: 'plugin',
  layerDataKind: 'raw-mask',
  pluginId: 'elegoo',
  formatVersions: [],
  settingsModes: [
    { value: 'simple', label: 'Simple', isDefault: true },
    { value: 'twostage', label: 'Two Stage' },
  ],
  rustModulePath: 'formats::goo',
  wasmExportName: 'encode_goo_container',
  notes: 'Goo binary encoder using big-endian raw mask layers.',
};
