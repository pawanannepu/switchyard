// ESLint for every TS and JS file in the pnpm workspace (AGENTS.md "Tooling").
// Type aware: typescript-eslint strict type checked, plus jsx-a11y (strict)
// and react-hooks for the UI. Prettier owns formatting, so its config goes last.

import js from "@eslint/js";
import { defineConfig } from "eslint/config";
import prettier from "eslint-config-prettier";
import jsxA11y from "eslint-plugin-jsx-a11y";
import reactHooks from "eslint-plugin-react-hooks";
import globals from "globals";
import tseslint from "typescript-eslint";

export default defineConfig(
  {
    ignores: [
      "**/node_modules/",
      "**/dist/",
      "target/",
      "apps/desktop/src-tauri/",
      "packages/protocol-ts/src/generated/",
      "**/playwright-report/",
      "**/test-results/",
    ],
  },

  js.configs.recommended,
  tseslint.configs.strictTypeChecked,
  {
    languageOptions: {
      parserOptions: {
        projectService: true,
        tsconfigRootDir: import.meta.dirname,
      },
    },
    rules: {
      // Exhaustive switches (AGENTS.md): a `default` doesn't excuse a missing union member.
      "@typescript-eslint/switch-exhaustiveness-check": [
        "error",
        { considerDefaultExhaustiveForUnions: false, requireDefaultForNonUnion: true },
      ],
      "@typescript-eslint/consistent-type-imports": "error",
      "@typescript-eslint/no-import-type-side-effects": "error",
    },
  },

  // Plain JS (scripts, this file): no type information.
  {
    files: ["**/*.{js,mjs,cjs}"],
    extends: [tseslint.configs.disableTypeChecked],
    languageOptions: { globals: globals.node },
  },

  // The React UI.
  {
    files: ["apps/desktop/src/**/*.{ts,tsx}"],
    extends: [jsxA11y.flatConfigs.strict, reactHooks.configs.flat.recommended],
    languageOptions: { globals: globals.browser },
  },

  // Node side TypeScript: configs and the Playwright suite.
  {
    files: ["**/*.config.ts", "apps/desktop/e2e/**/*.ts"],
    languageOptions: { globals: globals.node },
  },

  prettier,
);
