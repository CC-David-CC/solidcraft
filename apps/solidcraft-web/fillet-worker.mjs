import init, { compute_curvature_fillet } from './solidcraft_web.js';

self.addEventListener('message', async ({ data: [bytes, parameters, module] }) => {
  try {
    await init({ module_or_path: module });
    const result = compute_curvature_fillet(bytes, parameters);
    self.postMessage(result, [result.buffer]);
  } catch (error) {
    self.postMessage(String(error));
  }
}, { once: true });
