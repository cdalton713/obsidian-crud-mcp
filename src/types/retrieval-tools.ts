import { z } from "zod";
import { ScanParametersSchema } from "./note-scan.js";

export const ListTasksParametersSchema = z.object({
    ...ScanParametersSchema.shape,
    status: z.enum(["incomplete", "completed", "all"]).default("incomplete"),
});

export const GetNoteOutlineParametersSchema = z.object({ path: z.string().min(1).max(1000) });

export const SearchNotesParametersSchema = z.object({
    ...ScanParametersSchema.shape,
    query: z
        .string()
        .min(1)
        .max(200)
        .refine(
            (value) => value.trim().length > 0 && !/[\r\n]/.test(value),
            "Use a non-empty single-line phrase.",
        ),
    case_sensitive: z.boolean().default(false),
});

export const ReadNotesParametersSchema = z.object({
    paths: z.array(z.string().min(1).max(1000)).min(1).max(20),
    max_chars: z.number().int().min(1024).max(50_000).default(20_000),
});

export interface ReadResult {
    path: string;
    status: "ok" | "not_found" | "error" | "truncated";
    url?: string;
    content?: string;
    omitted_chars?: number;
}
