import pandas as pd
import matplotlib.pyplot as plt
from io import StringIO

# Legacy monad-bft and cosmos fork log message mapping.
COMMITTED_BLOCK = {
    ('monad_ledger', 'committed block'),
    ('monad_cosmos_ledger', 'committed cosmos block'),
}
ROUND_PROGRESS = {
    ('monad_consensus_state', 'Received Proposal'),
    ('monad_consensus::vote_state', 'Created new QC'),
}
VOTE_COLLECTION = {
    ('monad_consensus::vote_state', 'collecting vote'),
}
QC_FORMED = {
    ('monad_consensus::vote_state', 'Created new QC'),
}
PROPOSE_START = {
    ('monad_consensus_state', 'Creating Proposal'),
}
PROPOSE_DONE = {
    ('monad_eth_txpool', 'created proposal'),  # target prefix match
    ('monad_txpool::executor', 'cosmos txpool: proposal txs from PrepareProposal'),
}
PROPOSE_FAILED = {
    ('monad_consensus_state', "no eth_header found, can't propose"),
}
VOTE_CREATED = {
    ('monad_consensus_state', 'created vote'),
}
TIMEOUT = {
    ('monad_consensus_state', 'local timeout'),
}


class BftLog:
    def __init__(self, df):
        df['timestamp'] = pd.to_datetime(df['timestamp'])
        df['message'] = df['fields'].apply(lambda x: x.pop('message'))
        self.df = df

    def _target_message_mask(self, rules, prefix_targets=frozenset()):
        mask = pd.Series(False, index=self.df.index)
        for target, message in rules:
            target_mask = (
                self.df['target'].str.startswith(target)
                if target in prefix_targets
                else self.df['target'] == target
            )
            mask |= target_mask & (self.df['message'] == message)
        return mask

    def _field_int(self, series, key):
        return pd.to_numeric(
            series.apply(lambda x: x.get(key) if isinstance(x, dict) else None),
            errors='coerce',
        )

    def _rounds_from_rules(self, rules):
        df = self.df[self._target_message_mask(rules)]
        if df.empty:
            return pd.Series(dtype=int)
        return self._field_int(df['fields'], 'round').dropna().astype(int)

    def _missing_rounds(self, rounds):
        if rounds.empty:
            return []
        full_range = set(range(int(rounds.min()), int(rounds.max()) + 1))
        return sorted(full_range - set(rounds))

    ##############################################################################
    #                     DATA PARSING HELPER FUNCTIONS                          #
    ##############################################################################

    def block_commit_df(self):
        df = self.df[self._target_message_mask(COMMITTED_BLOCK)].copy()
        df = df[['timestamp', 'fields']]
        df['num_tx'] = df['fields'].apply(lambda x: x.get('num_tx'))
        df['block_num'] = df['fields'].apply(lambda x: x.get('block_num'))
        df = df.drop('fields', axis=1)

        df['duration'] = df['timestamp'].diff().dt.total_seconds() * 1000
        return df.dropna(subset=['duration'])

    def received_proposal_df(self):
        df = self.df[self._target_message_mask(ROUND_PROGRESS)].copy()
        df = df[['timestamp', 'fields']]
        df['round'] = self._field_int(df['fields'], 'round').astype(int)
        df = df.drop('fields', axis=1)

        df['duration'] = df['timestamp'].diff().dt.total_seconds() * 1000
        return df.dropna(subset=['duration'])

    def received_votes_df(self):
        df = self.df[self._target_message_mask(VOTE_COLLECTION)].copy()
        if not df.empty:
            df = df[['timestamp', 'fields']]
            df['round'] = self._field_int(df['fields'], 'round').astype(int)
            df = df.drop('fields', axis=1)

            df_grouped = df.groupby('round').agg({
                'timestamp': ['min', 'max', 'count']
            }).reset_index()
            df_grouped.columns = ['round', 'min_timestamp', 'max_timestamp', 'total_votes']
            df_grouped['timestamp_diff'] = (
                df_grouped['max_timestamp'] - df_grouped['min_timestamp']
            )
            df = df_grouped[['total_votes', 'round', 'timestamp_diff']].copy()
            df['duration'] = df['timestamp_diff'].dt.total_seconds() * 1000
            return df.drop('timestamp_diff', axis=1).round({'duration': 2})

        # Cosmos fallback: one QC per round; use inter-QC interval as round latency.
        df = self.df[self._target_message_mask(QC_FORMED)].copy()
        if df.empty:
            return df

        df = df[['timestamp', 'fields']]
        df['round'] = self._field_int(df['fields'], 'round').astype(int)
        df = df.drop('fields', axis=1).sort_values('round')
        df['duration'] = df['timestamp'].diff().dt.total_seconds() * 1000
        df['total_votes'] = 1
        return df.dropna(subset=['duration'])[['total_votes', 'round', 'duration']]

    def create_proposal_df(self):
        prefix_targets = {'monad_eth_txpool'}
        start_mask = self._target_message_mask(PROPOSE_START)
        done_mask = self._target_message_mask(PROPOSE_DONE, prefix_targets=prefix_targets)
        df = self.df[start_mask | done_mask].copy()
        if df.empty:
            return df

        df = df[['timestamp', 'fields', 'target', 'message']]
        df['seq_num'] = df.apply(
            lambda row: (
                row['fields'].get('try_propose_seq_num')
                if row['message'] == 'Creating Proposal'
                else row['fields'].get('proposed_seq_num', row['fields'].get('seq_num'))
            ),
            axis=1,
        )
        df['num_tx'] = df.apply(
            lambda row: (
                0
                if row['message'] == 'Creating Proposal'
                else row['fields'].get('proposal_num_tx', row['fields'].get('n_included', 0))
            ),
            axis=1,
        )
        df = df.drop(['fields', 'target', 'message'], axis=1)
        df['seq_num'] = pd.to_numeric(df['seq_num'], errors='coerce')
        df = df.dropna(subset=['seq_num'])
        df['seq_num'] = df['seq_num'].astype(int)
        df['num_tx'] = df['num_tx'].astype(int)

        df_grouped = df.groupby('seq_num').agg({
            'num_tx': ['max'],
            'timestamp': ['min', 'max'],
        }).reset_index()
        df_grouped.columns = ['seq_num', 'num_tx', 'min_timestamp', 'max_timestamp']
        df_grouped['timestamp_diff'] = (
            df_grouped['max_timestamp'] - df_grouped['min_timestamp']
        )
        df = df_grouped[['seq_num', 'num_tx', 'timestamp_diff']].copy()
        df['duration'] = df['timestamp_diff'].dt.total_seconds() * 1000
        return df.drop('timestamp_diff', axis=1).round({'duration': 2})

    def propose_failure_df(self):
        df = self.df[self._target_message_mask(PROPOSE_FAILED)].copy()
        if df.empty:
            return df

        df = df[['timestamp', 'fields']]
        df['round'] = self._field_int(df['fields'], 'round').astype(int)
        df['seq_num'] = self._field_int(df['fields'], 'try_propose_seq_num').astype(int)
        return df.drop('fields', axis=1)

    def missing_proposal_or_vote_df(self):
        rounds_proposal = self._rounds_from_rules(ROUND_PROGRESS)
        rounds_vote = self._rounds_from_rules(VOTE_CREATED)

        if rounds_proposal.empty:
            print("Missing proposal rounds: (no Received Proposal / Created new QC logs found)")
        else:
            print("Missing proposal rounds:", self._missing_rounds(rounds_proposal))

        if rounds_vote.empty:
            print(
                "Missing voting rounds: (no created vote logs found; "
                "enable debug logging for vote events)"
            )
        else:
            print("Missing voting rounds:", self._missing_rounds(rounds_vote))

        failures = self.propose_failure_df()
        if not failures.empty:
            print(f"Propose failures (no eth_header): {len(failures)} events")

    def timeout_df(self):
        proposal_messages = {msg for _, msg in ROUND_PROGRESS}
        df = self.df[
            self._target_message_mask(TIMEOUT)
            | self._target_message_mask(ROUND_PROGRESS)
        ].copy()
        if df.empty:
            return df

        df = df[['timestamp', 'message', 'fields']]
        df['round'] = self._field_int(df['fields'], 'round').astype(int)
        df = df.drop('fields', axis=1).reset_index(drop=True)

        last_timeout_timestamp = None
        last_timeout_round = None
        results = []

        for _, row in df.iterrows():
            if row['message'] == 'local timeout':
                if last_timeout_timestamp is None:
                    last_timeout_timestamp = row['timestamp']
                    last_timeout_round = row['round']
            elif row['message'] in proposal_messages and last_timeout_timestamp is not None:
                duration = (
                    row['timestamp'] - last_timeout_timestamp
                ).total_seconds() * 1000
                results.append({
                    'timeout_timestamp': last_timeout_timestamp,
                    'proposal_timestamp': row['timestamp'],
                    'round': last_timeout_round,
                    'duration': duration,
                })
                last_timeout_timestamp = None

        return pd.DataFrame(results)

    ##############################################################################
    #                     VISUALIZATION HELPER FUNCTIONS                         #
    ##############################################################################

    def _skip_if_empty(self, df, label):
        if df.empty:
            print(f"Skipping {label}: no matching log entries")
            return True
        return False

    def plot_block_commit(self):
        df = self.block_commit_df()
        if self._skip_if_empty(df, 'block_time.png'):
            return

        plt.figure(figsize=(12, 6))
        plt.scatter(df['block_num'], df['duration'], color='blue', s=5)
        plt.xlabel('Block Number')
        plt.ylabel('Duration (ms)')
        plt.title('Duration between consecutive blocks')
        plt.savefig('block_time.png')
        plt.close()

    def plot_received_proposal(self):
        df = self.received_proposal_df()
        if self._skip_if_empty(df, 'proposal_time.png'):
            return

        plt.figure(figsize=(12, 6))
        plt.scatter(df['round'], df['duration'], color='blue', s=5)
        plt.xlabel('Round')
        plt.ylabel('Duration (ms)')
        plt.title('Duration between consecutive proposals / QCs')
        plt.savefig('proposal_time.png')
        plt.close()

    def plot_received_votes(self):
        df = self.received_votes_df()
        if self._skip_if_empty(df, 'vote_collection.png'):
            return

        fig, ax1 = plt.subplots(figsize=(12, 6))
        color = 'tab:blue'
        ax1.set_xlabel('Round')
        ax1.set_ylabel('Duration (ms)', color=color)
        ax1.plot(df['round'], df['duration'], color=color)
        ax1.tick_params(axis='y', labelcolor=color)

        if df['total_votes'].nunique() > 1:
            ax2 = ax1.twinx()
            color = 'tab:orange'
            ax2.set_ylabel('Total Votes', color=color)
            ax2.plot(df['round'], df['total_votes'], color=color)
            ax2.tick_params(axis='y', labelcolor=color)
            plt.title('Duration taken to collect votes by leader')
        else:
            plt.title('Duration between consecutive QCs (cosmos fallback)')

        fig.tight_layout()
        plt.savefig('vote_collection.png')
        plt.close()

    def plot_create_proposal(self):
        df = self.create_proposal_df()
        if not df.empty and df['duration'].max() > 0:
            if self._skip_if_empty(df, 'proposal_creation.png'):
                return
            plt.figure(figsize=(12, 6))
            plt.plot(df['seq_num'], df['duration'], color='blue')
            plt.xlabel('Sequence number')
            plt.ylabel('Duration (ms)')
            plt.title('Duration for creating proposals')
            plt.savefig('proposal_creation.png')
            plt.close()
            return

        failures = self.propose_failure_df()
        if self._skip_if_empty(failures, 'proposal_creation.png'):
            return

        plt.figure(figsize=(12, 6))
        plt.scatter(failures['seq_num'], failures['round'], color='red', s=10)
        plt.xlabel('Sequence number')
        plt.ylabel('Round')
        plt.title("Propose failures (no eth_header found, can't propose)")
        plt.savefig('proposal_creation.png')
        plt.close()

    def plot_timeout(self):
        df = self.timeout_df()
        if self._skip_if_empty(df, 'timeout_duration.png'):
            return

        plt.figure(figsize=(12, 6))
        plt.scatter(df['round'], df['duration'], color='blue', s=50)
        plt.xlabel('Round')
        plt.ylabel('Duration (ms)')
        plt.title('Duration between timeouts and next proposal / QC')
        plt.savefig('timeout_duration.png')
        plt.close()

    @staticmethod
    def from_json(filepath_or_buffer):
        if hasattr(filepath_or_buffer, 'read'):
            lines = filepath_or_buffer
        else:
            lines = open(filepath_or_buffer)

        json_lines = []
        for line in lines:
            stripped = line.strip().lstrip('\x00')
            if stripped.startswith('{'):
                json_lines.append(stripped)

        if not json_lines:
            raise ValueError('no JSON log lines found in input')

        df = pd.read_json(StringIO('\n'.join(json_lines)), lines=True)
        return BftLog(df)
