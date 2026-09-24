type FormHandle = { setData: (data: unknown) => void };
type FormsLib = {
  init: (container: HTMLElement, schema: unknown, data: unknown, onChange?: (data: any) => void) => Promise<FormHandle | undefined>;
  parseSchema: (schema: unknown) => Promise<unknown>;
};
type CachedForm = { key: string; handle: FormHandle; onChange?: (data: any) => void };

// `init` re-parses and re-compiles the schema on every call (~0.5s for the config schema),
// so each container keeps a parsed form per schema variant and only swaps the data.
const formsByContainer = new WeakMap<HTMLElement, Map<string, CachedForm>>();
const MAX_FORMS_PER_CONTAINER = 8;
const displayedKey = new WeakMap<HTMLElement, string>();
// The library validates against one global validator: whichever schema was parsed last.
let validatorKey: string | null = null;
let parseQueue: Promise<unknown> = Promise.resolve();

function enqueue<T>(task: () => Promise<T>): Promise<T> {
  const run = parseQueue.then(task);
  parseQueue = run.catch(() => undefined);
  return run;
}

export async function initSchemaForm(
  forms: FormsLib,
  container: HTMLElement,
  schema: unknown,
  data: unknown,
  onChange?: (data: any) => void,
) {
  const key = JSON.stringify(schema);
  let cache = formsByContainer.get(container);
  if (!cache) formsByContainer.set(container, (cache = new Map()));
  let form = cache.get(key);
  if (!form) {
    const entry: CachedForm = { key, handle: undefined as unknown as FormHandle };
    // Hydrating with `{}` keeps the parsed tree pristine; `setData` hydrates from that tree.
    const handle = await enqueue(async () => {
      const created = await forms.init(container, schema, {}, (next) => entry.onChange?.(next));
      if (created) validatorKey = key;
      return created;
    });
    if (!handle) return;
    entry.handle = handle;
    form = entry;
    if (cache.size >= MAX_FORMS_PER_CONTAINER) cache.delete(cache.keys().next().value!);
    cache.set(key, form);
  } else if (validatorKey !== key) {
    // Recompiling blocks the main thread, so it waits until the form is actually used.
    const recompile = () => {
      container.removeEventListener("pointerdown", recompile, true);
      container.removeEventListener("focusin", recompile, true);
      if (displayedKey.get(container) !== key) return;
      void enqueue(async () => {
        if (validatorKey === key) return;
        await forms.parseSchema(schema);
        validatorKey = key;
      }).catch(() => undefined);
    };
    container.addEventListener("pointerdown", recompile, true);
    container.addEventListener("focusin", recompile, true);
  }
  displayedKey.set(container, key);
  form.onChange = onChange;
  form.handle.setData(data);
}
