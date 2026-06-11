#!/usr/bin/env node
import { readdirSync, readFileSync, writeFileSync } from "fs";
import { resolve } from "path";

const assetsDir = resolve("./assets");
const files = readdirSync(assetsDir);

const tag = process.env.GITHUB_REF_NAME;
const [owner, repo] = process.env.GITHUB_REPOSITORY.split("/");
const baseUrl = `https://github.com/${owner}/${repo}/releases/download/${tag}`;

const platforms = {};

for (const file of files) {
  if (!file.endsWith(".sig")) continue;

  const sig = readFileSync(resolve(assetsDir, file), "utf-8").trim();
  const bundleFile = file.replace(/\.sig$/, "");

  let platform;
  if (bundleFile.endsWith(".msi")) {
    platform = "windows-x86_64";
  } else if (bundleFile.includes("aarch64") && bundleFile.endsWith(".app.tar.gz")) {
    platform = "darwin-aarch64";
  } else if (bundleFile.includes("x64") && bundleFile.endsWith(".app.tar.gz")) {
    platform = "darwin-x86_64";
  } else if (bundleFile.endsWith(".AppImage.tar.gz")) {
    platform = "linux-x86_64";
  } else {
    console.log("Skipping unknown bundle:", bundleFile);
    continue;
  }

  platforms[platform] = {
    signature: sig,
    url: `${baseUrl}/${bundleFile}`,
  };
}

const json = {
  version: tag.replace(/^v/, ""),
  notes: `Release ${tag}`,
  pub_date: new Date().toISOString(),
  platforms,
};

writeFileSync("latest.json", JSON.stringify(json, null, 2));
console.log("Generated latest.json with platforms:", Object.keys(platforms).join(", "));
