self.addEventListener('message', async ({ data: [bytes, parameters, module, bindings] }) => {
  try {
    const { default: init, compute_curvature_fillet } = await import(bindings);
    await init({ module_or_path: module });
    const result = compute_curvature_fillet(bytes, parameters);
    self.postMessage(result, [result.buffer]);
  } catch (error) {
    self.postMessage(String(error));
  }
}, { once: true });
