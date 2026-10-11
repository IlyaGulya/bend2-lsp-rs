using System.Globalization;
using System.Text;
using System.Xml;
using Microsoft.Diagnostics.Tracing;
using Microsoft.Diagnostics.Tracing.Parsers.Kernel;

internal static class Program
{
    private const string Usage = "bend2-etl-reader <input.etl> <output.xml> <target-pid> <cpu|heap>";

    public static int Main(string[] args)
    {
        try
        {
            if (args.Length == 1 && args[0] == "--help")
            {
                Console.WriteLine(Usage);
                return 0;
            }
            if (args.Length != 4 || !int.TryParse(args[2], NumberStyles.None,
                    CultureInfo.InvariantCulture, out int pid) || pid <= 0 ||
                args[3] is not ("cpu" or "heap"))
                throw new ArgumentException(Usage);
            if (!OperatingSystem.IsWindows())
                throw new PlatformNotSupportedException("ETL decoding requires Windows");
            Decoder.Export(Path.GetFullPath(args[0]), Path.GetFullPath(args[1]), pid, args[3]);
            return 0;
        }
        catch (Exception error)
        {
            Console.Error.WriteLine($"ETL decode failed: {error.Message}");
            return 1;
        }
    }
}

internal static class Decoder
{
    private static readonly Guid Header = new("68fdd900-4a3e-11d1-84f4-0000f80464e3");
    private static readonly Guid Cpu = new("ce1dbfb4-137e-4da6-87b0-3f59aa102cbc");
    private static readonly Guid Thread = new("3d6fa8d1-fe05-11d0-9dda-00c04fd7ba7c");
    private static readonly Guid Heap = new("222962ab-6180-4b88-a825-346b75f2a24a");

    public static void Export(string input, string output, int pid, string kind)
    {
        string partial = output + ".partial";
        if (File.Exists(output))
            throw new IOException("Decoder output already exists");
        using var source = new ETWTraceEventSource(input, TraceEventSourceType.FileOnly);
        int headers = 0;
        ulong candidateEvents = 0;
        using (var stream = new FileStream(partial, FileMode.CreateNew, FileAccess.Write, FileShare.Read))
        using (var writer = XmlWriter.Create(stream, new XmlWriterSettings {
            Encoding = new UTF8Encoding(false), Indent = false, CloseOutput = false }))
        {
            writer.WriteStartElement("Events");
            source.Kernel.EventTraceHeader += data => {
                if (data.EventsLost != 0 || data.BuffersLost != 0)
                    throw new InvalidDataException("ETL header reports lost events/buffers");
                BeginEvent(writer, data, Header);
                Field(writer, "PointerSize", data.PointerSize);
                Field(writer, "EventsLost", data.EventsLost);
                Field(writer, "BuffersLost", data.BuffersLost);
                EndEvent(writer);
                headers++;
            };
            source.Kernel.LostEvent += _ =>
                throw new InvalidDataException("ETL contains lost-event notifications");
            source.Kernel.ThreadStart += data => ThreadEvent(writer, data);
            source.Kernel.ThreadStop += data => ThreadEvent(writer, data);
            source.Kernel.ThreadDCStart += data => ThreadEvent(writer, data);
            source.Kernel.ThreadDCStop += data => ThreadEvent(writer, data);
            source.Kernel.PerfInfoSample += data => {
                if (data.Version != 2 || data.EventDataLength != source.PointerSize + 8)
                    throw new InvalidDataException("Unsupported or truncated CPU sample payload");
                if (data.InstructionPointer == 0)
                    throw new InvalidDataException("CPU sample omitted its instruction pointer");
                BeginEvent(writer, data, Cpu);
                Field(writer, "InstructionPointer", data.InstructionPointer);
                Field(writer, "ThreadId", data.ThreadID);
                EndEvent(writer);
                // Target ownership is validated from lifecycle payloads by Rust,
                // not from TraceEvent's possibly unresolved reporter ProcessID.
                if (kind == "cpu")
                    candidateEvents = checked(candidateEvents + 1);
            };
            source.Dynamic.All += data => {
                if (kind != "heap" || data.ProviderGuid != Heap ||
                    (int)data.Opcode != 33 || data.ProcessID != pid)
                    return;
                BeginEvent(writer, data, Heap);
                foreach (string name in data.PayloadNames)
                    Field(writer, name, data.PayloadByName(name));
                EndEvent(writer);
                candidateEvents = checked(candidateEvents + 1);
            };
            // All callbacks run through the complete input. Never stop after the
            // first target event: late losses and malformed records still matter.
            if (!source.Process())
                throw new InvalidDataException("ETL processing did not reach EOF");
            if (headers == 0 || source.EventsLost != 0)
                throw new InvalidDataException("Missing header or nonzero ETL event loss");
            if (candidateEvents == 0)
                throw new InvalidDataException($"ETL has no candidate {kind} events");
            writer.WriteEndElement();
            writer.Flush();
        }
        File.Move(partial, output);
    }

    private static void ThreadEvent(XmlWriter writer, ThreadTraceData data)
    {
        BeginEvent(writer, data, Thread);
        Field(writer, "ProcessId", data.ProcessID);
        Field(writer, "ThreadId", data.ThreadID);
        EndEvent(writer);
    }

    private static void BeginEvent(XmlWriter writer, TraceEvent data, Guid eventGuid)
    {
        writer.WriteStartElement("Event");
        writer.WriteStartElement("System");
        writer.WriteStartElement("Provider");
        writer.WriteAttributeString("Guid", data.ProviderGuid.ToString("D"));
        writer.WriteEndElement();
        writer.WriteElementString("Version", data.Version.ToString(CultureInfo.InvariantCulture));
        writer.WriteElementString("Opcode", ((int)data.Opcode).ToString(CultureInfo.InvariantCulture));
        writer.WriteStartElement("Execution");
        if (data.ProcessID >= 0)
            writer.WriteAttributeString("ProcessID", data.ProcessID.ToString(CultureInfo.InvariantCulture));
        if (data.ThreadID >= 0)
            writer.WriteAttributeString("ThreadID", data.ThreadID.ToString(CultureInfo.InvariantCulture));
        writer.WriteEndElement();
        writer.WriteEndElement();
        writer.WriteStartElement("ExtendedTracingInfo");
        writer.WriteElementString("EventGuid", eventGuid.ToString("D"));
        writer.WriteEndElement();
        writer.WriteStartElement("EventData");
    }

    private static void Field(XmlWriter writer, string name, object? value)
    {
        writer.WriteStartElement("Data");
        writer.WriteAttributeString("Name", name);
        writer.WriteString(Convert.ToString(value, CultureInfo.InvariantCulture) ?? "");
        writer.WriteEndElement();
    }

    private static void EndEvent(XmlWriter writer)
    {
        writer.WriteEndElement();
        writer.WriteEndElement();
    }
}
