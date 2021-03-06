use crate::{
    batch::{Batch, BatchReader, BatchWriter},
    idl::{
        IngestionDataSharePacket, IngestionHeader, InvalidPacket, Packet, SumPart,
        ValidationHeader, ValidationPacket,
    },
    transport::{SignableTransport, VerifiableAndDecryptableTransport, VerifiableTransport},
    BatchSigningKey, Error,
};
use anyhow::{anyhow, Context, Result};
use chrono::NaiveDateTime;
use prio::server::{Server, VerificationMessage};
use std::convert::TryFrom;
use uuid::Uuid;

pub struct BatchAggregator<'a> {
    is_first: bool,
    aggregation_name: &'a str,
    aggregation_start: &'a NaiveDateTime,
    aggregation_end: &'a NaiveDateTime,
    own_validation_transport: &'a mut VerifiableTransport,
    peer_validation_transport: &'a mut VerifiableTransport,
    ingestion_transport: &'a mut VerifiableAndDecryptableTransport,
    aggregation_batch: BatchWriter<'a, SumPart, InvalidPacket>,
    share_processor_signing_key: &'a BatchSigningKey,
}

impl<'a> BatchAggregator<'a> {
    #[allow(clippy::too_many_arguments)] // Grandfathered in
    pub fn new(
        aggregation_name: &'a str,
        aggregation_start: &'a NaiveDateTime,
        aggregation_end: &'a NaiveDateTime,
        is_first: bool,
        ingestion_transport: &'a mut VerifiableAndDecryptableTransport,
        own_validation_transport: &'a mut VerifiableTransport,
        peer_validation_transport: &'a mut VerifiableTransport,
        aggregation_transport: &'a mut SignableTransport,
    ) -> Result<BatchAggregator<'a>> {
        Ok(BatchAggregator {
            is_first,
            aggregation_name,
            aggregation_start,
            aggregation_end,
            own_validation_transport,
            peer_validation_transport,
            ingestion_transport,
            aggregation_batch: BatchWriter::new(
                Batch::new_sum(
                    aggregation_name,
                    aggregation_start,
                    aggregation_end,
                    is_first,
                ),
                &mut *aggregation_transport.transport,
            ),
            share_processor_signing_key: &aggregation_transport.batch_signing_key,
        })
    }

    /// Compute the sum part for all the provided batch IDs and write it out to
    /// the aggregation transport.
    pub fn generate_sum_part(&mut self, batch_ids: &[(Uuid, NaiveDateTime)]) -> Result<()> {
        let mut invalid_uuids = Vec::new();

        let ingestion_header = self.ingestion_header(&batch_ids[0].0, &batch_ids[0].1)?;

        // Ideally, we would use the encryption_key_id in the ingestion packet
        // to figure out which private key to use for decryption, but that field
        // is optional. Instead we try all the keys we have available until one
        // works.
        // https://github.com/abetterinternet/prio-server/issues/73
        let mut servers = self
            .ingestion_transport
            .packet_decryption_keys
            .iter()
            .map(|k| Server::new(ingestion_header.bins as usize, self.is_first, k.clone()))
            .collect::<Vec<Server>>();

        for batch_id in batch_ids {
            self.aggregate_share(&batch_id.0, &batch_id.1, &mut servers, &mut invalid_uuids)?;
        }

        // TODO(timg) what exactly do we write out when there are no invalid
        // packets? Right now we will write an empty file.
        let invalid_packets_digest =
            self.aggregation_batch
                .packet_file_writer(|mut packet_file_writer| {
                    for invalid_uuid in invalid_uuids {
                        InvalidPacket { uuid: invalid_uuid }.write(&mut packet_file_writer)?
                    }
                    Ok(())
                })?;

        // We have one Server for each packet decryption key, and each of those
        // instances could contain some accumulated shares, depending on which
        // key was used to encrypt an individual packet. We make a new Server
        // instance into which we will aggregate them all together. It doesn't
        // matter which private key we use here as we're not decrypting any
        // packets with this Server instance, just accumulating data vectors.
        let mut accumulator_server = Server::new(
            ingestion_header.bins as usize,
            self.is_first,
            self.ingestion_transport.packet_decryption_keys[0].clone(),
        );
        for server in servers.iter() {
            accumulator_server.merge_total_shares(server.total_shares());
        }

        let sum = accumulator_server
            .total_shares()
            .iter()
            .map(|f| u32::from(*f) as i64)
            .collect();

        let total_individual_clients = accumulator_server.total_shares().len() as i64;

        let sum_signature = self.aggregation_batch.put_header(
            &SumPart {
                batch_uuids: batch_ids.iter().map(|pair| pair.0).collect(),
                name: ingestion_header.name,
                bins: ingestion_header.bins,
                epsilon: ingestion_header.epsilon,
                prime: ingestion_header.prime,
                number_of_servers: ingestion_header.number_of_servers,
                hamming_weight: ingestion_header.hamming_weight,
                sum,
                aggregation_start_time: self.aggregation_start.timestamp_millis(),
                aggregation_end_time: self.aggregation_end.timestamp_millis(),
                packet_file_digest: invalid_packets_digest.as_ref().to_vec(),
                total_individual_clients,
            },
            &self.share_processor_signing_key.key,
        )?;

        self.aggregation_batch
            .put_signature(&sum_signature, &self.share_processor_signing_key.identifier)
    }

    /// Fetch the ingestion header from one of the batches so various parameters
    /// may be read from it.
    fn ingestion_header(
        &mut self,
        batch_id: &Uuid,
        batch_date: &NaiveDateTime,
    ) -> Result<IngestionHeader> {
        let mut ingestion_batch: BatchReader<'_, IngestionHeader, IngestionDataSharePacket> =
            BatchReader::new(
                Batch::new_ingestion(self.aggregation_name, batch_id, batch_date),
                &mut *self.ingestion_transport.transport.transport,
            );
        let ingestion_header = ingestion_batch
            .header(&self.ingestion_transport.transport.batch_signing_public_keys)?;
        Ok(ingestion_header)
    }

    /// Aggregate the batch for the provided batch_id into the provided server.
    /// The UUIDs of packets for which aggregation fails are recorded in the
    /// provided invalid_uuids vector.
    fn aggregate_share(
        &mut self,
        batch_id: &Uuid,
        batch_date: &NaiveDateTime,
        servers: &mut Vec<Server>,
        invalid_uuids: &mut Vec<Uuid>,
    ) -> Result<()> {
        let mut ingestion_batch: BatchReader<'_, IngestionHeader, IngestionDataSharePacket> =
            BatchReader::new(
                Batch::new_ingestion(self.aggregation_name, batch_id, batch_date),
                &mut *self.ingestion_transport.transport.transport,
            );
        let mut own_validation_batch: BatchReader<'_, ValidationHeader, ValidationPacket> =
            BatchReader::new(
                Batch::new_validation(self.aggregation_name, batch_id, batch_date, self.is_first),
                &mut *self.own_validation_transport.transport,
            );
        let mut peer_validation_batch: BatchReader<'_, ValidationHeader, ValidationPacket> =
            BatchReader::new(
                Batch::new_validation(self.aggregation_name, batch_id, batch_date, !self.is_first),
                &mut *self.peer_validation_transport.transport,
            );
        let peer_validation_header = peer_validation_batch
            .header(&self.peer_validation_transport.batch_signing_public_keys)?;
        let own_validation_header = own_validation_batch
            .header(&self.own_validation_transport.batch_signing_public_keys)?;
        let ingestion_header = ingestion_batch
            .header(&self.ingestion_transport.transport.batch_signing_public_keys)?;

        // Make sure all the parameters in the headers line up
        if !peer_validation_header.check_parameters(&own_validation_header) {
            return Err(anyhow!(
                "validation headers do not match. Peer: {:?}\nOwn: {:?}",
                peer_validation_header,
                own_validation_header
            ));
        }
        if !ingestion_header.check_parameters(&peer_validation_header) {
            return Err(anyhow!(
                "ingestion header does not match peer validation header. Ingestion: {:?}\nPeer:{:?}",
                ingestion_header,
                peer_validation_header
            ));
        }

        let mut peer_validation_packet_reader =
            peer_validation_batch.packet_file_reader(&peer_validation_header)?;
        let mut own_validation_packet_reader =
            own_validation_batch.packet_file_reader(&own_validation_header)?;
        let mut ingestion_packet_reader = ingestion_batch.packet_file_reader(&ingestion_header)?;

        loop {
            let peer_validation_packet =
                match ValidationPacket::read(&mut peer_validation_packet_reader) {
                    Ok(p) => Some(p),
                    Err(Error::EofError) => None,
                    Err(e) => return Err(e.into()),
                };
            let own_validation_packet =
                match ValidationPacket::read(&mut own_validation_packet_reader) {
                    Ok(p) => Some(p),
                    Err(Error::EofError) => None,
                    Err(e) => return Err(e.into()),
                };
            let ingestion_packet =
                match IngestionDataSharePacket::read(&mut ingestion_packet_reader) {
                    Ok(p) => Some(p),
                    Err(Error::EofError) => None,
                    Err(e) => return Err(e.into()),
                };

            // All three packet files should contain the same number of packets,
            // so if any of the readers hit EOF before the others, something is
            // fishy.
            let (peer_validation_packet, own_validation_packet, ingestion_packet) = match (
                &peer_validation_packet,
                &own_validation_packet,
                &ingestion_packet,
            ) {
                (Some(a), Some(b), Some(c)) => (a, b, c),
                (None, None, None) => break,
                (_, _, _) => {
                    return Err(anyhow!(
                        "unexpected early EOF when checking peer validations"
                    ));
                }
            };

            // TODO(timg) we need to make sure we are evaluating a valid triple
            // of (peer validation, own validation, ingestion), i.e., they must
            // have the same UUID. Can we assume that the peer validations will
            // be in the same order as ours, or do we need to do an O(n) search
            // of the peer validation packets to find the right UUID? Further,
            // if aggregation fails, then we record the invalid UUID and send
            // that along to the aggregator. But if some UUIDs are missing from
            // the peer validation packet file (the EOF case handled above),
            // should that UUID be marked as invalid or do we abort handling of
            // the whole batch?
            // For now I am assuming that all batches maintain the same order
            // and that they are required to contain the same set of UUIDs.
            if peer_validation_packet.uuid != own_validation_packet.uuid
                || peer_validation_packet.uuid != ingestion_packet.uuid
                || own_validation_packet.uuid != ingestion_packet.uuid
            {
                return Err(anyhow!(
                    "mismatch between peer validation, own validation and ingestion packet UUIDs: {} {} {}",
                    peer_validation_packet.uuid,
                    own_validation_packet.uuid,
                    ingestion_packet.uuid));
            }

            let mut did_aggregate_shares = false;
            let mut last_err = None;
            for server in servers.iter_mut() {
                match server.aggregate(
                    &ingestion_packet.encrypted_payload,
                    &VerificationMessage::try_from(peer_validation_packet)?,
                    &VerificationMessage::try_from(own_validation_packet)?,
                ) {
                    Ok(valid) => {
                        if !valid {
                            invalid_uuids.push(peer_validation_packet.uuid);
                        }
                        did_aggregate_shares = true;
                        break;
                    }
                    Err(e) => {
                        last_err = Some(Err(e));
                        continue;
                    }
                }
            }
            if !did_aggregate_shares {
                return last_err
                    // Unwrap the optional, providing an error if it is None
                    .context("unknown validation error")?
                    // Wrap either the default error or what we got from
                    // server.aggregate
                    .context("failed to validate packets");
            }
        }

        Ok(())
    }
}
