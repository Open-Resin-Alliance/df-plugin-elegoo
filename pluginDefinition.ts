import type {
  ComplexPluginDefinition,
  PluginLocalMaterialSettingsAdapterContract,
} from '@/features/plugins/complexPluginContracts';
import { ELEGOO_PLUGIN_MANIFEST } from './pluginManifest';
import { GOO_FORMAT_DEFINITION } from './slicing/gooFormatDefinition';
import gooSimpleMaterialSettings from './materialSettings/settings_simple.json';
import gooTwostageMaterialSettings from './materialSettings/settings_twostage.json';

type GooModeSettingsSchema = Omit<PluginLocalMaterialSettingsAdapterContract, 'outputFormat'>;

function createGooModeSettingsAdapter(schema: GooModeSettingsSchema): PluginLocalMaterialSettingsAdapterContract {
  return { outputFormat: GOO_FORMAT_DEFINITION.outputFormat, ...schema };
}

const GOO_LOCAL_MATERIAL_SETTINGS_SIMPLE_ADAPTER = createGooModeSettingsAdapter(
  gooSimpleMaterialSettings as GooModeSettingsSchema,
);
const GOO_LOCAL_MATERIAL_SETTINGS_TWOSTAGE_ADAPTER = createGooModeSettingsAdapter(
  gooTwostageMaterialSettings as GooModeSettingsSchema,
);

export const ELEGOO_COMPLEX_PLUGIN_DEFINITION: ComplexPluginDefinition = {
  id: 'elegoo',
  manifest: ELEGOO_PLUGIN_MANIFEST,
  capabilities: {
    networkOperations: false,
    uploadWithProgress: false,
    slicerEncoder: true,
    tauriRuntimePlugin: false,
  },
  slicingFormatsByOutput: {
    [GOO_FORMAT_DEFINITION.outputFormat]: GOO_FORMAT_DEFINITION,
  },
  localMaterialSettingsByOutput: {
    [GOO_FORMAT_DEFINITION.outputFormat]: GOO_LOCAL_MATERIAL_SETTINGS_SIMPLE_ADAPTER,
  },
  localMaterialSettingsByOutputAndMode: {
    [GOO_FORMAT_DEFINITION.outputFormat]: {
      simple: GOO_LOCAL_MATERIAL_SETTINGS_SIMPLE_ADAPTER,
      twostage: GOO_LOCAL_MATERIAL_SETTINGS_TWOSTAGE_ADAPTER,
    },
  },
};
export default ELEGOO_COMPLEX_PLUGIN_DEFINITION;
