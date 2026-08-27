# Release checklist

1. **Bump Cargo version.** Set the same version in [Cargo.toml](Cargo.toml) and the `taskvisor` entry in [Cargo.lock](Cargo.lock).

2. **Update documentation.** Describe the release changes in the guide and examples.

3. **Check CI.** Confirm CI has passed and the release changes are merged into `main`.

4. **Create a GitHub Release.** Open [New release](https://github.com/soltiHQ/taskvisor/releases/new), select `main` as the target, and set both tag and title to `vX.Y.Z`.
   Click [Generate release notes](https://docs.github.com/en/repositories/releasing-projects-on-github/automatically-generated-release-notes#creating-automatically-generated-release-notes-for-a-new-release) to fill the description/changelog, then **Publish release**.

5. **Check publication.** Wait for [Tag publish](https://github.com/soltiHQ/taskvisor/actions/workflows/tag-publish.yml) and the linked documentation site run to succeed.
   Verify the new version on [crates.io](https://crates.io/crates/taskvisor) and [docs.rs](https://docs.rs/taskvisor).

## Docs checklist

Run from the Taskvisor repository root.

1. **Clone site** next to Taskvisor, unless it is already cloned:

   ```bash
   git clone https://github.com/soltiHQ/site.git ../site
   ```

2. **Generate Taskvisor docs:**

   ```bash
   export TASK_X_REMOTE_TASKFILES=1
   TASKVISOR_SOURCE="$PWD"
   task ci/docs
   ```

3. **Build the guide and start the site:**

   ```bash
   cd ../site
   task ci/docs-build SOURCE="$TASKVISOR_SOURCE"
   cd app
   npm ci
   npm run dev
   ```

4. **Open [Taskvisor docs](http://localhost:5173/docs/taskvisor/).** Use the port printed by Vite if it differs.

After edits, rerun both docs build commands and refresh the page.
