// Vite inline-worker imports (`import W from './x?worker&inline'`). Bundles the worker into the
// importing module as a self-contained Blob, so the single vendor bundle needs no extra file.
declare module '*?worker&inline' {
    const workerConstructor: { new (): Worker };
    export default workerConstructor;
}
