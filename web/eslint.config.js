import js from '@eslint/js';
import tseslint from 'typescript-eslint';
import reactHooks from 'eslint-plugin-react-hooks';
import reactRefresh from 'eslint-plugin-react-refresh';
import prettier from 'eslint-config-prettier';
import globals from 'globals';

export default tseslint.config(
  // public/ is the wasm client glue, served as-is; astryx-themes.js is generated.
  {ignores: ['dist', 'public', 'src/astryx-themes.js']},
  {
    files: ['**/*.{ts,tsx}'],
    extends: [js.configs.recommended, ...tseslint.configs.recommended, reactHooks.configs.flat.recommended],
    languageOptions: {globals: globals.browser},
    plugins: {'react-refresh': reactRefresh},
    rules: {
      'react-refresh/only-export-components': ['warn', {allowConstantExport: true}],
      '@typescript-eslint/no-unused-vars': ['error', {argsIgnorePattern: '^_'}],
    },
  },
  // Formatting is Prettier's job.
  prettier,
);
