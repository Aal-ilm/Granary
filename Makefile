# Client setup for this machine: send npm, pip, Cargo, Maven, Gradle and git
# through Granary, or switch them back to the public registries (for example
# while Granary is down). Works with the GNU Make 3.81 that ships with macOS.
#
#   make use-granary GRANARY_URL=http://10.212.46.20:8480   first time
#   make use-public                                          Granary down
#   make use-granary                                         back again
#   make status
#   make remove
#   make fix-lockfile DIR=~/code/app                         see below
#
# npm writes Granary's address into package-lock.json. While on the public
# registries, run fix-lockfile on a project before installing; it switches
# those URLs to registry.npmjs.org, which also work through Granary.
#
# Limit to some tools with TOOLS, e.g.  make use-public TOOLS=npm
# TOOLS=maven covers both Maven (~/.m2/settings.xml) and Gradle
# (~/.gradle/init.d/granary.gradle). TOOLS=git sends git clones of GitHub,
# GitLab and Codeberg repos (and Cargo git dependencies) through Granary.

CLIENT := ./scripts/granary-client.sh

# Pass these through to the script when given on the make command line.
export GRANARY_URL GRANARY_TOKEN TOOLS

.PHONY: help use-granary use-public status remove fix-lockfile

DIR ?= .

help:
	@echo "make use-granary GRANARY_URL=http://HOST:PORT   send npm, pip, cargo, maven, gradle and git through Granary"
	@echo "make use-public                                  use the public registries directly"
	@echo "make status                                      show where each tool points"
	@echo "make remove                                      remove all Granary settings and the saved token"
	@echo "make fix-lockfile DIR=PATH                       make a project's package-lock.json work without Granary"
	@echo ""
	@echo "Optional: TOOLS=\"npm pip cargo maven git\" to choose tools (maven includes gradle), GRANARY_TOKEN=... to skip the prompt."

use-granary:
	@bash $(CLIENT) use-granary

use-public:
	@bash $(CLIENT) use-public

status:
	@bash $(CLIENT) status

remove:
	@bash $(CLIENT) remove

fix-lockfile:
	@bash $(CLIENT) fix-lockfile "$(DIR)"
