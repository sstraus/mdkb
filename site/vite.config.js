import { defineConfig } from "vite";
import { readFileSync } from "fs";
import { resolve } from "path";

// `<!-- @include NAME -->` → diagrams/NAME.svg, one file per animated diagram.
function diagrams() {
  return {
    name: "diagrams",
    transformIndexHtml: {
      order: "pre",
      handler: (html) =>
        html.replace(/<!-- @include ([a-z-]+) -->/g, (_, name) =>
          readFileSync(resolve(__dirname, "diagrams", `${name}.svg`), "utf8"),
        ),
    },
  };
}

export default defineConfig({
  base: "/",
  plugins: [diagrams()],
  build: { outDir: "dist" },
});
