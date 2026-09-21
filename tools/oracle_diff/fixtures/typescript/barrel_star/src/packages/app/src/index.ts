import { greet } from '@acme/ui';

export function boot(): string {
    return greet();   // through wildcard re-export
}
