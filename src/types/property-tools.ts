import { z } from "zod";

export const ScalarSchema = z.union([z.string(), z.number().finite(), z.boolean(), z.null()]);

export const PropertyKeySchema = z
    .string()
    .min(1)
    .max(200)
    .refine((key) => key.trim().length > 0, "Property names cannot be blank.");

export const UpdateNotePropertiesParametersSchema = z
    .object({
        path: z.string().min(1).max(1000),
        set: z
            .record(PropertyKeySchema, z.union([ScalarSchema, z.array(ScalarSchema).max(1000)]))
            .default({}),
        remove: z.array(PropertyKeySchema).max(100).default([]),
    })
    .refine(
        (args) => Object.keys(args.set).length > 0 || args.remove.length > 0,
        "Supply properties to set or remove.",
    );
