import { mkdirSync, copyFileSync } from "node:fs";
const root = new URL("./", import.meta.url);
mkdirSync(new URL("dist/", root), { recursive: true });
for (const file of ["index.html", "app.js", "model.js", "style.css"])
  copyFileSync(new URL(file, root), new URL(`dist/${file}`, root));
