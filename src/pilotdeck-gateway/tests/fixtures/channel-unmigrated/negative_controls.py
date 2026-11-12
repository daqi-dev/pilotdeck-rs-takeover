#!/usr/bin/env python3
"""#1386 A8.3 unmigrated-channel rejection controls; compile each mutant first.

OUT [mutation ...], --compile-only OUT, --check (also --check-anchors).
The shared runner owns source restoration, offline cargo, nonce receipts,
patch evidence, pending registrations and the complete result summary.

H1 keeps all 23 fault rows. Three historical rejection cells move to the
production-loader tests documented in README.md; they are not claimed as
kills of the remaining rejection gate.
"""
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from negative_suite import matrix_main

root = Path(__file__).resolve().parents[5]
source = root / 'src/pilotdeck-gateway/src/channel'
# Migration preserves all 23 fault classes; per-adapter production mount
# and reinserted-rejection controls live in channel-loader-startup.
MUTATIONS = [('qq-dropped',
  source / 'loader.rs',
  'pub const UNMIGRATED_CHANNEL_KEYS: &[&str] = &[\n    "qq",',
  'pub const UNMIGRATED_CHANNEL_KEYS: &[&str] = &[',
  [('rejects_unmigrated_qq', '#1386 unmigrated channel must reject startup')]),
 ('rejection-silenced',
  source / 'loader.rs',
  'return Err(ChannelError::error(format!(\n'
  '                "[adapters] channel \\"{key}\\" 未迁移到 Rust，需继续使用 TS gateway"\n'
  '            )));',
  'continue;',
  [('rejects_unmigrated_qq', '#1386 unmigrated channel must reject startup'),
   ('rejects_unmigrated_wecom', '#1386 unmigrated channel must reject startup')]),
 ('rejection-message-loses-key',
  source / 'loader.rs',
  '"[adapters] channel \\"{key}\\" 未迁移到 Rust，需继续使用 TS gateway"',
  '"[adapters] 未迁移到 Rust，需继续使用 TS gateway"',
  'rejects_unmigrated_qq',
  '#1386 rejection names the channel key'),
 ('subset-sms-rejected',
  source / 'loader.rs',
  '    "qq",\n    "wecom",\n];',
  '    "qq",\n    "wecom",\n    "sms",\n];',
  'unmigrated_subset_channels_stay_loadable',
  '#1386 subset channel must not be rejected'),
 ('reject-table-reinserts-slack',
  source / 'loader.rs',
  'pub const UNMIGRATED_CHANNEL_KEYS: &[&str] = &[\n    "qq",',
  'pub const UNMIGRATED_CHANNEL_KEYS: &[&str] = &[\n    "slack",\n    "qq",',
  'unmigrated_subset_channels_stay_loadable',
  '#1386 subset channel must not be rejected'),
 ('qq-wecom-gate-loosened',
  source / 'loader.rs',
  'matches!(cfg.get("enabled"), Some(JsValue::Bool(true)))',
  'js_truthy(cfg.get("enabled"))',
  'unmigrated_bundle_channels_require_strict_boolean_true',
  '#1386 bundle channel requires strict enabled === true'),
 ('strict-key-dropped',
  source / 'loader.rs',
  'const STRICT_TRUE_CHANNEL_KEYS: &[&str] = &["qq", "wecom"];',
  'const STRICT_TRUE_CHANNEL_KEYS: &[&str] = &["wecom"];',
  'strict_true_channel_keys_are_exactly_qq_and_wecom',
  '#1386 strict-true channel key set must be exactly qq and wecom'),
 ('loader-key-made-strict',
  source / 'loader.rs',
  'const STRICT_TRUE_CHANNEL_KEYS: &[&str] = &["qq", "wecom"];',
  'const STRICT_TRUE_CHANNEL_KEYS: &[&str] = &["qq", "wecom", "telegram"];',
  'strict_true_channel_keys_are_exactly_qq_and_wecom',
  '#1386 strict-true channel key set must be exactly qq and wecom'),
 ('strict-gate-always-true',
  source / 'loader.rs',
  'matches!(cfg.get("enabled"), Some(JsValue::Bool(true)))',
  'true',
  'unmigrated_falsy_or_absent_enabled_does_not_reject',
  '#1386 disabled unmigrated channel must not reject'),
 ('loader-table-iteration',
  source / 'loader.rs',
  'for key in UNMIGRATED_CHANNEL_KEYS {',
  'for key in CHANNEL_LOADER_KEYS {',
  [('rejects_unmigrated_qq', '#1386 unmigrated channel must reject startup'),
   ('rejects_unmigrated_wecom', '#1386 unmigrated channel must reject startup')]),
 ('loader-table-gains-ungrouped-key',
  source / 'loader.rs',
  'pub const CHANNEL_LOADER_KEYS: [&str; 15] = [\n    "telegram",',
  'pub const CHANNEL_LOADER_KEYS: [&str; 16] = [\n    "probe1853",\n    "telegram",',
  'loader_keys_partition_rejected_and_implemented',
  '#2301 every loader key is rejected or implemented'),
 ('loader-table-loses-silent-sms',
  source / 'loader.rs',
  'pub const CHANNEL_LOADER_KEYS: [&str; 15] = [\n'
  '    "telegram",\n'
  '    "discord",\n'
  '    "slack",\n'
  '    "matrix",\n'
  '    "mattermost",\n'
  '    "signal",\n'
  '    "whatsapp",\n'
  '    "bluebubbles",\n'
  '    "dingtalk",\n'
  '    "wecomCallback",\n'
  '    "email",\n'
  '    "sms",',
  'pub const CHANNEL_LOADER_KEYS: [&str; 14] = [\n'
  '    "telegram",\n'
  '    "discord",\n'
  '    "slack",\n'
  '    "matrix",\n'
  '    "mattermost",\n'
  '    "signal",\n'
  '    "whatsapp",\n'
  '    "bluebubbles",\n'
  '    "dingtalk",\n'
  '    "wecomCallback",\n'
  '    "email",',
  'loader_keys_partition_rejected_and_implemented',
  '#2301 every implemented loader key is a loader-table key'),
 ('rejection-table-gains-foreign-key',
  source / 'loader.rs',
  '    "qq",\n    "wecom",\n];',
  '    "probe1853",\n    "qq",\n    "wecom",\n];',
  'loader_keys_partition_rejected_and_implemented',
  '#1853 every rejection-table key is a loader key or a strict bundle key'),
 ('bundle-gains-silent-sms',
  source / 'loader.rs',
  'pub const BUNDLE_CHANNEL_KEYS: &[&str] = &["weixin", "feishu"];',
  'pub const BUNDLE_CHANNEL_KEYS: &[&str] = &["weixin", "feishu", "sms"];',
  'loader_keys_partition_rejected_and_implemented',
  '#1853 bundle keys stay outside the loader table'),
 ('native-mount-loses-loader-key',
  source / 'loader.rs',
  'pub const CHANNEL_LOADER_KEYS: [&str; 15] = [\n    "telegram",\n    "discord",\n    "slack",',
  'pub const CHANNEL_LOADER_KEYS: [&str; 14] = [\n    "telegram",\n    "discord",',
  'loader_keys_partition_rejected_and_implemented',
  '#2301 every implemented loader key is a loader-table key'),
 ('implemented-gains-rejected-qq',
  source / 'tests.rs',
  'const IMPLEMENTED_LOADER_KEYS: [&str; 15] = [\n        "slack",',
  'const IMPLEMENTED_LOADER_KEYS: [&str; 16] = [\n        "qq",\n        "slack",',
  'loader_keys_partition_rejected_and_implemented',
  '#2301 every implemented loader key is outside the rejection table'),
 ('subset-whitelist-loses-silent-sms',
  source / 'tests.rs',
  'const SUBSET_CHANNEL_KEYS: [&str; 17] = [\n'
  '    "weixin",\n'
  '    "feishu",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "sms",\n'
  '    "bluebubbles",\n'
  '    "slack",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'const SUBSET_CHANNEL_KEYS: [&str; 16] = [\n'
  '    "weixin",\n'
  '    "feishu",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "bluebubbles",\n'
  '    "slack",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'loader_keys_partition_rejected_and_implemented',
  '#2301 every implemented loader key is in the module subset whitelist'),
 ('subset-whitelist-gains-foreign-key',
  source / 'tests.rs',
  'const SUBSET_CHANNEL_KEYS: [&str; 17] = [\n'
  '    "weixin",\n'
  '    "feishu",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "sms",\n'
  '    "bluebubbles",\n'
  '    "slack",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'const SUBSET_CHANNEL_KEYS: [&str; 18] = [\n'
  '    "probe1853",\n'
  '    "weixin",\n'
  '    "feishu",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "sms",\n'
  '    "bluebubbles",\n'
  '    "slack",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'loader_keys_partition_rejected_and_implemented',
  '#2301 every module subset key is implemented or a bundle key'),
 ('subset-whitelist-loses-slack',
  source / 'tests.rs',
  'const SUBSET_CHANNEL_KEYS: [&str; 17] = [\n'
  '    "weixin",\n'
  '    "feishu",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "sms",\n'
  '    "bluebubbles",\n'
  '    "slack",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'const SUBSET_CHANNEL_KEYS: [&str; 16] = [\n'
  '    "weixin",\n'
  '    "feishu",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "sms",\n'
  '    "bluebubbles",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'loader_keys_partition_rejected_and_implemented',
  '#2301 every implemented loader key is in the module subset whitelist'),
 ('subset-whitelist-loses-bundle-weixin',
  source / 'tests.rs',
  'const SUBSET_CHANNEL_KEYS: [&str; 17] = [\n'
  '    "weixin",\n'
  '    "feishu",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "sms",\n'
  '    "bluebubbles",\n'
  '    "slack",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'const SUBSET_CHANNEL_KEYS: [&str; 16] = [\n'
  '    "feishu",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "sms",\n'
  '    "bluebubbles",\n'
  '    "slack",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'loader_keys_partition_rejected_and_implemented',
  '#2367 every bundle key is in the module subset whitelist'),
 ('subset-whitelist-loses-bundle-feishu',
  source / 'tests.rs',
  'const SUBSET_CHANNEL_KEYS: [&str; 17] = [\n'
  '    "weixin",\n'
  '    "feishu",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "sms",\n'
  '    "bluebubbles",\n'
  '    "slack",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'const SUBSET_CHANNEL_KEYS: [&str; 16] = [\n'
  '    "weixin",\n'
  '    "apiServer",\n'
  '    "webhook",\n'
  '    "wecomCallback",\n'
  '    "sms",\n'
  '    "bluebubbles",\n'
  '    "slack",\n'
  '    "signal",\n'
  '    "mattermost",\n'
  '    "homeassistant",\n'
  '    "email",\n'
  '    "dingtalk",\n'
  '    "discord",\n'
  '    "matrix",\n'
  '    "telegram",\n'
  '    "whatsapp",\n'
  '];',
  'loader_keys_partition_rejected_and_implemented',
  '#2367 every bundle key is in the module subset whitelist')]


def main():
    guarded = list(source.glob('*.rs'))
    args = ['--check'] if sys.argv[1:] == ['--check-anchors'] else sys.argv[1:]
    return matrix_main(root, __file__, 'channel::tests::', MUTATIONS,
                       passed_count=1, child_count=0, paths=guarded,
                       exact=True, args=args,
                       any_cell=('rejection-silenced', 'loader-table-iteration'))


if __name__ == '__main__':
    raise SystemExit(main())
