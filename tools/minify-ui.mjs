// Regenerates ui/index.html (the minified page embedded in the exe) from ui/index.src.html.
// Needs esbuild:  npm i -g esbuild   then   node tools/minify-ui.mjs
import { readFileSync, writeFileSync } from "node:fs";
import { transformSync } from "esbuild";

const src = readFileSync(new URL("../ui/index.src.html", import.meta.url), "utf8");
const css = /<style>([\s\S]*?)<\/style>/;
const js = /<script>([\s\S]*?)<\/script>/;
const cm = src.match(css), jm = src.match(js);
if (!cm || !jm) throw new Error("index.src.html needs one <style> and one <script>");

// Top-level names stay as written (inline onclick="..." handlers call them); only inner locals are shortened.
const c = transformSync(cm[1], { loader: "css", minify: true }).code.trim();
const j = transformSync(jm[1], { loader: "js", minify: true, target: "es2020", legalComments: "none" }).code.trim();

let out = src.replace(css, () => `<style>${c}</style>`).replace(js, () => `<script>${j}</script>`);
out = out.replace(/>\s*\n\s*</g, "><").replace(/\n+/g, "\n");
writeFileSync(new URL("../ui/index.html", import.meta.url), out);
console.log(`ui/index.html: ${src.length} -> ${out.length} bytes`);
