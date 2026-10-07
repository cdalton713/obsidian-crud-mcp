import { z } from "zod";

export const PropertyValueSchema = z.union([
    z.string(),
    z.number(),
    z.boolean(),
    z.null(),
    z.array(z.union([z.string(), z.number(), z.boolean(), z.null()])),
]);
export type PropertyValue = z.infer<typeof PropertyValueSchema>;
