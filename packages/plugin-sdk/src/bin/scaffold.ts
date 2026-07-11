#!/usr/bin/env bun
import { resolve } from "node:path"

import { scaffoldTypeScriptPlugin } from "../scaffold"

const destination = process.argv[2]
if (destination === undefined || destination.startsWith("-")) {
  console.error("usage: rottweiler-plugin-scaffold <directory> [name]")
  process.exit(2)
}
const name = process.argv[3]
await scaffoldTypeScriptPlugin(resolve(destination), name === undefined ? {} : { name })
