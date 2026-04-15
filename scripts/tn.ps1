# tn.ps1 — PowerShell wrapper around the `therminal` CLI.
#
# Windows equivalent of scripts/tn (bash). See that file for the
# CLI-vs-MCP usage policy and full subcommand reference.
#
# Installation: copy this file (or the tn.exe alias) to a directory
# on $env:PATH. The .exe copy is preferred — it works without
# execution-policy concerns.

& therminal @args
