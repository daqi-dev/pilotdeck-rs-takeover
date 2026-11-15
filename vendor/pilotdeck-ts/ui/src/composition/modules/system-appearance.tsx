import type { FrontendModule } from '../contracts';
import AppearanceSettings from '../../components/settings/view/appearance';

const module: FrontendModule = {
  id: 'system.appearance',
  businessModuleId: 'system.appearance',
  contract: 'pilotdeck.business/v1',
  source: 'pilotdeck',
  frontendApiVersion: 'frontend-module/v1',
  settings: [{
    id: 'system-appearance',
    settingsSection: 'system-appearance',
    label: 'Appearance',
    labelKey: 'settingsPage.menu.appearance',
    component: AppearanceSettings,
  }],
};

export default module;
