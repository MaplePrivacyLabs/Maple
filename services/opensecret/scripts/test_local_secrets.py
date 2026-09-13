"""Offline contract tests; never read Keychain or contact BWS."""
import os
import io
from pathlib import Path
import unittest
from unittest.mock import patch
import tomllib

import local_secrets as runner


class LocalSecretsTests(unittest.TestCase):
    def test_removes_ambient_routing_and_credentials_preserves_workspace(self):
        inherited = {
            'PATH': '/tools', 'DATABASE_URL': 'local-db', 'JWT_SECRET': 'workspace-fixture',
            'CONTINUUM_PROXY_PORT': '12345', 'SECRETSPEC_PROVIDER': 'env',
            'SECRETSPEC_FILE': '/wrong', 'SECRETSPEC_SCOPE': 'wrong',
            'SECRETSPEC_BWS_CLI_PATH': '/wrong', 'BWS_ACCESS_TOKEN': 'bootstrap-fixture',
            'BWS_SERVER_URL': 'https://wrong.invalid', 'BWS_CONFIG_FILE': '/wrong',
            **{name: 'ambient-fixture' for name in runner.PROVIDER_KEYS},
        }
        result = runner.clean_environment(inherited, '/tools/bws')
        self.assertEqual(result['DATABASE_URL'], 'local-db')
        self.assertEqual(result['JWT_SECRET'], 'workspace-fixture')
        self.assertEqual(result['CONTINUUM_PROXY_PORT'], '12345')
        self.assertEqual(result['SECRETSPEC_BWS_CLI_PATH'], '/tools/bws')
        self.assertFalse(any(k.startswith('BWS_') for k in result))
        self.assertEqual([k for k in result if k.startswith('SECRETSPEC_')], ['SECRETSPEC_BWS_CLI_PATH'])
        for key in ('CONTINUUM_API_KEY', 'KAGI_API_KEY', 'TINFOIL_API_KEY'):
            self.assertNotIn(key, result)
        self.assertEqual(result['BRAVE_API_KEY'], '')
        self.assertEqual(result['OPENAI_API_KEY'], '')

    def test_exec_scopes_child_without_bootstrap_or_shell_interpolation(self):
        with patch.dict(os.environ, {'BWS_ACCESS_TOKEN': 'bootstrap-fixture'}, clear=True), \
             patch.object(runner.shutil, 'which', side_effect=lambda x: '/tools/' + x), \
             patch.object(runner.os, 'execve') as execute:
            runner.main(['run', 'backend', '--', 'example', 'argument with spaces', '$(literal)'])
        executable, args, env = execute.call_args.args
        self.assertEqual(executable, '/tools/secretspec')
        self.assertEqual(args[args.index('--scope') + 1], 'backend')
        self.assertEqual(args[-4:], ['--', 'example', 'argument with spaces', '$(literal)'])
        self.assertNotIn('BWS_ACCESS_TOKEN', env)
        self.assertIn('--file', args)
        self.assertIn('--reason', args)

    def test_check_cannot_prompt_or_write_missing_values(self):
        args = runner.command_args('/tools/secretspec', 'check', 'local', [])
        self.assertIn('--no-prompt', args)
        self.assertNotIn('set', args)
        self.assertNotIn('login', args)

    def test_login_is_explicit_and_reuses_provider_credential_location(self):
        args = runner.command_args('/tools/secretspec', 'login', 'local', [])
        self.assertEqual(args[-4:], ['config', 'provider', 'login', 'local_bws'])
        with patch.object(runner.sys.stdin, 'isatty', return_value=False), patch('sys.stderr', new_callable=io.StringIO):
            with self.assertRaises(SystemExit):
                runner.main(['login'])

    def test_manifest_has_only_owned_keys_and_minimal_scopes(self):
        # The Nix check copies the manifest beside these test files.
        path = Path(__file__).resolve().parent.parent / 'secretspec.toml'
        if not path.exists():
            path = Path(__file__).resolve().parent / 'secretspec.toml'
        config = tomllib.loads(path.read_text())
        secrets = config['profiles']['default']
        self.assertEqual(set(secrets), {'CONTINUUM_API_KEY', 'TINFOIL_API_KEY', 'KAGI_API_KEY'})
        for name, declaration in secrets.items():
            self.assertTrue(declaration['required'])
            self.assertEqual(declaration['providers'], ['local_bws'])
            self.assertEqual(declaration['ref']['item'], name.lower())
            self.assertNotIn('generate', declaration)
            self.assertNotIn('default', declaration)
        self.assertEqual(config['scopes']['continuum']['secrets'], ['CONTINUUM_API_KEY'])
        self.assertEqual(set(config['scopes']['backend']['secrets']), {'TINFOIL_API_KEY', 'KAGI_API_KEY'})
        self.assertEqual(set(config['scopes']['local']['secrets']), set(secrets))


if __name__ == '__main__':
    unittest.main()
