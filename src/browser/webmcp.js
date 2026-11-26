// Run in the page's main world: an isolated world cannot see page adapters.
// Never print exceptions, source objects, arguments, or unfiltered results.
(async function ({op, index, input, confirmed, key, timeout, limit, registry, generation}) {
    const markers = ['agent_hidden', 'agent_confirm', 'agent_secret'];
    const marked = (value, marker) => value != null && typeof value === 'object' &&
        Object.hasOwn(value, marker) && value[marker] !== false;
    const hasMarker = (value, marker, depth = 0) => {
        if (value === null || typeof value !== 'object') return false;
        if (depth > 64) throw new Error();
        return marked(value, marker) || Object.values(value).some(v => hasMarker(v, marker, depth + 1));
    };
    const fields = tool => [tool, tool.annotations, tool.inputSchema, tool.outputSchema];
    const has = (tool, marker) => fields(tool).some(v => marked(v, marker));
    // In native WebIDL, unknown annotation members are dropped. Schema
    // extensions survive. A secret-bearing schema is not advertised verbatim.
    const secret = tool => has(tool, 'agent_secret') ||
        hasMarker(tool.inputSchema, 'agent_secret') || hasMarker(tool.outputSchema, 'agent_secret');
    const hidden = tool => has(tool, 'agent_hidden') ||
        hasMarker(tool.inputSchema, 'agent_hidden') || hasMarker(tool.outputSchema, 'agent_hidden');
    const requiresConfirmation = tool => has(tool, 'agent_confirm') || hasMarker(tool.inputSchema, 'agent_confirm') || hasMarker(tool.outputSchema, 'agent_confirm');
    const describe = tool => {
        const annotations = {};
        for (const hint of ['readOnlyHint', 'idempotentHint', 'destructiveHint', 'untrustedContentHint', 'consequentialHint', 'debugging']) {
            if (typeof tool.annotations?.[hint] === 'boolean') annotations[hint] = tool.annotations[hint];
        }
        if (requiresConfirmation(tool)) annotations.agent_confirm = true;
        if (secret(tool)) annotations.agent_secret = true;
        return {
            name: tool.name, description: secret(tool) ? '' : tool.description,
            inputSchema: secret(tool) ? null : sanitize(tool.inputSchema ?? null),
            annotations,
        };
    };
    const sanitize = (value, depth = 0) => {
        if (value === null || typeof value !== 'object') return value;
        if (depth > 64) throw new Error();
        if (marked(value, 'agent_hidden')) return undefined;
        if (marked(value, 'agent_secret')) return '[redacted]';
        // Semantic automation nodes declare a redacted value with `secret`.
        // Keep the opaque ref usable for writes, but discard other payload.
        if (Object.hasOwn(value, 'secret') && value.secret !== false) {
            return {secret: true, value: '[redacted]', ...(typeof value.ref === 'string' ? {ref: value.ref} : {})};
        }
        if (Array.isArray(value)) return value.map(v => sanitize(v, depth + 1)).filter(v => v !== undefined);
        return Object.fromEntries(Object.entries(value).flatMap(([k, v]) => {
            if (markers.includes(k)) return [];
            const clean = sanitize(v, depth + 1);
            return clean === undefined ? [] : [[k, clean]];
        }));
    };

    const controller = new AbortController();
    const signal = controller.signal;
    // Only a random invocation-scoped abort closure is published; arguments
    // and results are never stored on the global object.
    const abort = () => controller.abort();
    Object.defineProperty(globalThis, key, {value: abort, configurable: true});
    const timer = setTimeout(() => controller.abort(new DOMException('Deadline', 'TimeoutError')), timeout);
    const stopped = new Promise(resolve => signal.addEventListener('abort', () => {
        resolve({status: signal.reason?.name === 'TimeoutError' ? 'timeout' : 'cancelled'});
    }, {once: true}));
    const run = async () => {
        if (op === 'list') {
            const usable = ctx => ctx != null && typeof ctx.getTools === 'function' && typeof ctx.executeTool === 'function';
            const candidate = navigator.modelContext;
            const context = usable(candidate) ? candidate : document.modelContext;
            if (!usable(context)) return {status: 'ok', value: []};
            // Native getTools also discovers same-origin child frames. Never
            // request other origins, and keep only the top-level source.
            const tools = (await context.getTools()).filter(tool => {
                const source = 'source' in tool ? tool.source : ('window' in tool ? tool.window : undefined);
                const own = source === globalThis && tool.origin === globalThis.origin;
                const localAdapter = source === undefined && !('origin' in tool);
                return (own || localAdapter) && !hidden(tool);
            });
            signal.throwIfAborted();
            const value = tools.map(describe);
            // Do not resurrect discovery refs when this document is restored
            // from the back/forward cache. Replace the preceding listener too.
            const previous = globalThis[registry];
            if (previous) removeEventListener('pagehide', previous.clear);
            const clear = () => { delete globalThis[registry]; };
            addEventListener('pagehide', clear, {once: true});
            Object.defineProperty(globalThis, registry, {value: {generation, context, tools, clear}, configurable: true});
            return {status: 'ok', value};
        }
        const binding = globalThis[registry];
        if (binding?.generation !== generation) return {status: 'stale'};
        const {context, tools} = binding;
        // Keep the actual Tool descriptor so the native API rejects a removed
        // or replaced registration even when another tool reuses its name.
        const tool = tools[index];
        if (!tool || hidden(tool)) return {status: 'not_found'};
        if (secret(tool) || markers.some(marker => hasMarker(input, marker))) return {status: 'refused'};
        if (requiresConfirmation(tool) && confirmed !== true) {
            // No caller-supplied values or keys go back to the approval channel.
            const properties = tool.inputSchema?.properties ?? {};
            const args = Object.fromEntries(Object.keys(properties)
                .filter(k => Object.hasOwn(input, k)).map(k => [k, '[redacted]']));
            return {status: 'confirmation_required', tool: {
                name: tool.name, description: tool.description, arguments: args,
            }};
        }
        signal.throwIfAborted();
        let result = await context.executeTool(tool, input, {signal});
        signal.throwIfAborted();
        // Native WebMCP serializes objects to a string; adapters may return
        // the object directly. Unstructured string results remain strings.
        if (typeof result === 'string') {
            if (result.length > limit) return {status: 'too_large'};
            try {
                const parsed = JSON.parse(result);
                if (parsed !== null && typeof parsed === 'object') result = parsed;
            } catch { /* plain text */ }
        }
        return {status: 'ok', value: sanitize(result) ?? null};
    };
    try {
        const reply = JSON.stringify(await Promise.race([stopped, run()]));
        if (reply.length <= limit && new TextEncoder().encode(reply).length <= limit) return reply;
        return '{"status":"too_large"}';
    } catch (error) {
        let status;
        switch (error?.name) {
            case 'NotAllowedError': case 'SecurityError': status = 'refused'; break;
            case 'NotFoundError': status = 'not_found'; break;
            case 'InvalidStateError': status = 'stale'; break;
            default: status = 'failed';
        }
        return JSON.stringify({status});
    } finally {
        clearTimeout(timer);
        delete globalThis[key];
    }
})
