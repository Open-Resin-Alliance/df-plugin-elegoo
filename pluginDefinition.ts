import type {
    ComplexPluginDefinition,
    PluginLocalMaterialSettingsAdapterContract,
    MaterialSettingsSource,
} from '@/features/plugins/complexPluginContracts';
import { resolveDifferentialMaterialSettings } from '@/features/plugins/resolveDifferentialSettings';
import { ELEGOO_PLUGIN_MANIFEST } from './pluginManifest';
import { GOO_FORMAT_DEFINITION } from './slicing/gooFormatDefinition';
import gooSimpleMaterialSettings from './materialSettings/settings_simple.json';
import gooTwostageMaterialSettings from './materialSettings/settings_twostage.json';
import gooTwostageDiffMaterialSettings from './materialSettings/settings_twostage.diff.json';


function createGooModeSettingsAdapter(
    modeName: string,
    allModeSources: Record<string, MaterialSettingsSource>,
): PluginLocalMaterialSettingsAdapterContract {
    const source = allModeSources[modeName];
    if (!source) {
        throw new Error(`[Goo] Settings mode "${modeName}" not found in mode sources`);
    }
    const resolved = resolveDifferentialMaterialSettings(source, allModeSources);
    return { outputFormat: GOO_FORMAT_DEFINITION.outputFormat, ...resolved };
}

const GOO_MODE_SOURCES: Record<string, MaterialSettingsSource> = {
    simple: gooSimpleMaterialSettings as MaterialSettingsSource,
    twostage: gooTwostageDiffMaterialSettings as MaterialSettingsSource,
};

const GOO_LOCAL_MATERIAL_SETTINGS_SIMPLE_ADAPTER = createGooModeSettingsAdapter('simple', GOO_MODE_SOURCES);
const GOO_LOCAL_MATERIAL_SETTINGS_TWOSTAGE_ADAPTER = createGooModeSettingsAdapter('twostage', GOO_MODE_SOURCES);

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
