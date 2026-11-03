// SPDX-FileCopyrightText: 2026 Timothy Redaelli <timothy.redaelli@gmail.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0

// Writes an RVZSTD or TorrentZip zip from a directory using RomVault's own
// Compress library, as a byte-for-byte reference for rvsz.
// usage: Oracle <dir> <out.zip> [threads] [zstd|trrnt]
using System;
using System.Collections.Generic;
using System.IO;
using Compress;
using Compress.StructuredZip;
using SortMethods;

static class Program
{
    static void Walk(string root, string rel, List<(string name, string path)> list)
    {
        string dir = rel == "" ? root : Path.Combine(root, rel);
        bool empty = true;
        foreach (string p in Directory.EnumerateFileSystemEntries(dir))
        {
            empty = false;
            string name = rel == "" ? Path.GetFileName(p) : rel + "/" + Path.GetFileName(p);
            if (Directory.Exists(p))
                Walk(root, name, list);
            else
                list.Add((name, p));
        }
        if (empty && rel != "")
            list.Add((rel + "/", null));
    }

    static int Main(string[] args)
    {
        int threads = args.Length > 2 ? int.Parse(args[2]) : 4;
        ZipStructure st = (args.Length > 3 ? args[3] : "zstd") switch
        {
            "zstd" => ZipStructure.ZipZSTD,
            "trrnt" => ZipStructure.ZipTrrnt,
            string s => throw new ArgumentException("unknown structure " + s),
        };
        var list = new List<(string name, string path)>();
        Walk(args[0], "", list);
        list.Sort((a, b) => Sorters.TrrntZipStringCompareCase(a.name, b.name));

        var zip = new StructuredZip();
        ZipReturn zr = zip.ZipFileCreate(args[1], st);
        if (zr != ZipReturn.ZipGood) { Console.Error.WriteLine("create: " + zr); return 1; }
        byte[] buf = new byte[1 << 20];
        foreach (var (name, path) in list)
        {
            ulong size = path == null ? 0 : (ulong)new FileInfo(path).Length;
            zr = zip.ZipFileOpenWriteStream(false, name, size,
                StructuredArchive.GetCompressionType(st, size), out Stream ws, null, threads);
            if (zr != ZipReturn.ZipGood) { Console.Error.WriteLine(name + ": " + zr); return 1; }
            uint crc = 0;
            if (path != null)
            {
                var h = new System.IO.Hashing.Crc32();
                using var fs = File.OpenRead(path);
                int n;
                while ((n = fs.Read(buf, 0, buf.Length)) > 0)
                {
                    h.Append(buf.AsSpan(0, n));
                    ws.Write(buf, 0, n);
                }
                crc = h.GetCurrentHashAsUInt32();
            }
            ws.Flush();
            byte[] bcrc = { (byte)(crc >> 24), (byte)(crc >> 16), (byte)(crc >> 8), (byte)crc };
            zr = zip.ZipFileCloseWriteStream(bcrc);
            if (zr != ZipReturn.ZipGood) { Console.Error.WriteLine(name + ": " + zr); return 1; }
        }
        zip.ZipFileClose();
        return 0;
    }
}
