export const performance = globalThis.performance || { now:()=>Date.now(), timeOrigin: Date.now() };
export class PerformanceObserver { observe(){} disconnect(){} }
export class PerformanceEntry {}
export default { performance, PerformanceObserver, PerformanceEntry };
